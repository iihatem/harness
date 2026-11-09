//! A scripted child process for the crate's tests: it plays the part of `claude` or `codex`.
//!
//! `FAKE_CHILD_SCRIPT` names a file of directives, one per line:
//! `emit <text>` prints a line; `expect <substring>` reads a line of stdin and exits 97 when it
//! does not contain the text; `read` reads and logs one stdin line; `until-eof` reads and logs
//! stdin to its end; `stderr <text>` prints to stderr; `sleep <ms>`; `exit <code>`;
//! `ignore-sigint` and `ignore-sigterm`; `spawn-grandchild <pidfile>` starts a `sleep 300` in
//! the same process group and writes its pid; `hang` sleeps for ever. A `$LINE` in an `emit` is
//! replaced by the last line read. `FAKE_CHILD_LOG` receives `argv: <args joined by U+001F>`
//! first, then `in: <line>` for each stdin line read; `FAKE_CHILD_ENVDUMP` receives the
//! environment, one `NAME=value` per line.

use std::{
    fs::OpenOptions,
    io::{BufRead, Write},
    process::{Command, Stdio},
    time::Duration,
};

use nix::sys::signal::{SigHandler, Signal, signal};

fn log(line: &str) {
    if let Ok(path) = std::env::var("FAKE_CHILD_LOG") {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("log file");
        writeln!(file, "{line}").expect("log write");
    }
}

fn read_line(stdin: &mut impl BufRead) -> Option<String> {
    let mut line = String::new();
    match stdin.read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => {
            let line = line.trim_end_matches(['\n', '\r']).to_string();
            log(&format!("in: {line}"));
            Some(line)
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    log(&format!("argv: {}", args.join("\u{1f}")));
    if let Ok(path) = std::env::var("FAKE_CHILD_ENVDUMP") {
        let dump: String = std::env::vars()
            .map(|(k, v)| format!("{k}={v}\n"))
            .collect();
        std::fs::write(path, dump).expect("env dump");
    }
    let script = std::fs::read_to_string(std::env::var("FAKE_CHILD_SCRIPT").expect("script"))
        .expect("script file");
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let mut stdout = std::io::stdout();
    let mut last = String::new();
    for directive in script.lines() {
        let (word, rest) = directive.split_once(' ').unwrap_or((directive, ""));
        match word {
            "emit" => {
                writeln!(stdout, "{}", rest.replace("$LINE", &last)).expect("stdout");
                stdout.flush().expect("flush");
            }
            "expect" => match read_line(&mut stdin) {
                Some(line) if line.contains(rest) => last = line,
                got => {
                    eprintln!("expected a line with {rest:?}, got {got:?}");
                    std::process::exit(97);
                }
            },
            "read" => last = read_line(&mut stdin).unwrap_or_default(),
            "until-eof" => while read_line(&mut stdin).is_some() {},
            "stderr" => eprintln!("{rest}"),
            "sleep" => std::thread::sleep(Duration::from_millis(rest.parse().expect("ms"))),
            "exit" => std::process::exit(rest.parse().expect("code")),
            "ignore-sigint" => unsafe {
                signal(Signal::SIGINT, SigHandler::SigIgn).expect("signal");
            },
            "ignore-sigterm" => unsafe {
                signal(Signal::SIGTERM, SigHandler::SigIgn).expect("signal");
            },
            "spawn-grandchild" => {
                // Never waited for: it is the orphan the tests look for.
                #[allow(clippy::zombie_processes)]
                let child = Command::new("sleep")
                    .arg("300")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawn sleep");
                std::fs::write(rest, child.id().to_string()).expect("pidfile");
            }
            "hang" => loop {
                std::thread::sleep(Duration::from_secs(60));
            },
            "" => {}
            other => panic!("unknown directive {other}"),
        }
    }
}
