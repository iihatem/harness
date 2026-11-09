//! Which models a child offers: Codex's own `model/list`, asked of a short `codex app-server`
//! run, and the aliases Claude's `--help` names. Neither starts a task, reads a credential, or
//! needs the user to be signed in to ask.
//!
//! The answers are best effort and checked only against what the schema and the help text say:
//! a Codex that answers in a shape this does not read lists nothing (the caller says so), and a
//! Claude lists the aliases its help names and the three it has always had.

use std::{ffi::OsString, path::PathBuf, process::Stdio, time::Duration};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
};

use crate::{
    BackendKind, DelegateError,
    detect::{DetectOptions, detect},
    env::{AuthMode, child_env},
};

/// What to run and how long to wait for it.
#[derive(Debug, Clone)]
pub struct ListOptions {
    /// The binary to run; else `PATH` is searched.
    pub binary: Option<PathBuf>,
    pub path_var: Option<OsString>,
    pub parent_env: Option<Vec<(OsString, OsString)>>,
    /// Accept a version outside the tested range.
    pub allow_untested: bool,
    /// The whole listing, from spawning to the answer.
    pub timeout: Duration,
}

impl Default for ListOptions {
    fn default() -> ListOptions {
        ListOptions {
            binary: None,
            path_var: None,
            parent_env: None,
            allow_untested: false,
            timeout: Duration::from_secs(15),
        }
    }
}

impl ListOptions {
    fn detect(&self) -> DetectOptions {
        DetectOptions {
            binary: self.binary.clone(),
            path_var: self.path_var.clone(),
            allow_untested: self.allow_untested,
            ..DetectOptions::default()
        }
    }

    fn env(&self, kind: BackendKind) -> Vec<(OsString, OsString)> {
        let parent = self
            .parent_env
            .clone()
            .unwrap_or_else(|| std::env::vars_os().collect());
        child_env(kind, &AuthMode::Subscription, parent)
    }
}

/// The aliases Claude Code lists for `--model`, when its help names none.
pub const CLAUDE_DEFAULT_ALIASES: [&str; 3] = ["sonnet", "opus", "haiku"];

/// A model name is a short word of this set: it ends up in the picker and, later, in an
/// argument of the child, so nothing else is listed.
fn plausible(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:/@+-".contains(c))
}

/// The quoted aliases of the `--model` option in `help` (`'sonnet'`, `'opus'`), in order, once
/// each. The option's text runs to the next option.
pub fn parse_claude_aliases(help: &str) -> Vec<String> {
    let mut aliases: Vec<String> = Vec::new();
    let mut inside = false;
    for line in help.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("--model") || trimmed.contains(" --model ") {
            inside = true;
        } else if inside && trimmed.starts_with('-') {
            break;
        }
        if !inside {
            continue;
        }
        let mut rest = line;
        while let Some(start) = rest.find('\'') {
            let after = &rest[start + 1..];
            let Some(end) = after.find('\'') else { break };
            let word = &after[..end];
            // The alias is a lowercase word; a full model name in the example has digits and
            // dashes, and is a name rather than an alias.
            if word.chars().all(|c| c.is_ascii_lowercase()) && plausible(word) {
                let word = word.to_string();
                if !aliases.contains(&word) {
                    aliases.push(word);
                }
            }
            rest = &after[end + 1..];
        }
    }
    aliases
}

/// The aliases `claude --help` names for `--model`.
pub async fn claude_aliases(options: &ListOptions) -> Result<Vec<String>, DelegateError> {
    let found = detect(BackendKind::Claude, &options.detect()).await?;
    let spawn = |source| DelegateError::Spawn {
        binary: "claude".into(),
        source,
    };
    let mut command = Command::new(&found.path);
    command
        .arg("--help")
        .env_clear()
        .envs(options.env(BackendKind::Claude))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(spawn)?;
    let group = child.id().and_then(|id| i32::try_from(id).ok());
    let mut stdout = child.stdout.take().expect("piped");
    let read = async {
        let mut bytes = Vec::new();
        let _ = (&mut stdout).take(256 * 1024).read_to_end(&mut bytes).await;
        let _ = child.wait().await;
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let Ok(help) = tokio::time::timeout(options.timeout, read).await else {
        // A program that does not answer `--help` is not listed as usable, and is ended with
        // whatever it started.
        if let Some(group) = group {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(group),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        return Err(spawn(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "claude did not answer `--help` in time",
        )));
    };
    // What the help names, then the usual three it did not name (`haiku` is an alias whether or
    // not the help's example mentions it).
    let mut aliases = parse_claude_aliases(&help);
    for usual in CLAUDE_DEFAULT_ALIASES {
        if !aliases.iter().any(|a| a == usual) {
            aliases.push(usual.to_string());
        }
    }
    Ok(aliases)
}

/// The model names in one `model/list` result: `data[]`, each with a `model` (else an `id`),
/// not hidden.
pub fn parse_codex_models(result: &Value) -> Vec<String> {
    result["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["hidden"].as_bool() != Some(true))
        .filter_map(|m| m["model"].as_str().or_else(|| m["id"].as_str()))
        .filter(|name| plausible(name))
        .map(String::from)
        .collect()
}

/// The models `codex app-server` lists (`model/list`), asked of a run that is ended once it has
/// answered.
pub async fn codex_models(options: &ListOptions) -> Result<Vec<String>, DelegateError> {
    let found = detect(BackendKind::Codex, &options.detect()).await?;
    let spawn = |source| DelegateError::Spawn {
        binary: "codex".into(),
        source,
    };
    // As a turn does: the user's own MCP servers are switched off by name, and the run is in a
    // directory of its own, so no project config is read and nothing of the project is reached.
    let env = options.env(BackendKind::Codex);
    let neutral = tempfile::tempdir().map_err(spawn)?;
    let servers = crate::codex::user_mcp_servers(&found.path, &env, neutral.path()).await;
    let mut command = Command::new(&found.path);
    command
        .args(crate::codex::argv::build(&servers))
        .current_dir(neutral.path())
        .env_clear()
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(spawn)?;
    let group = child.id().and_then(|id| i32::try_from(id).ok());
    let mut stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    let exchange = async {
        let mut lines = BufReader::new(stdout).lines();
        let mut send = async |value: Value| -> std::io::Result<()> {
            stdin.write_all(format!("{value}\n").as_bytes()).await?;
            stdin.flush().await
        };
        send(json!({"id": 1, "method": "initialize", "params": {
            "clientInfo": {"name": "harness", "title": "harness", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {"experimentalApi": false},
        }}))
        .await?;
        let mut models: Vec<String> = Vec::new();
        let mut next_id = 2u64;
        let mut waiting_for = 1u64;
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        while let Some(line) = lines.next_line().await? {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if v["id"].as_u64() != Some(waiting_for) || v.get("method").is_some() {
                continue;
            }
            if waiting_for == 1 {
                send(json!({"method": "initialized"})).await?;
            } else {
                if v.get("error").is_some() {
                    return Ok(models);
                }
                for name in parse_codex_models(&v["result"]) {
                    if !models.contains(&name) {
                        models.push(name);
                    }
                }
                pages += 1;
                cursor = v["result"]["nextCursor"].as_str().map(String::from);
                if cursor.is_none() || pages >= 5 {
                    return Ok(models);
                }
            }
            let mut params = json!({"includeHidden": false, "limit": 100});
            if let Some(cursor) = &cursor {
                params["cursor"] = json!(cursor);
            }
            waiting_for = next_id;
            next_id += 1;
            send(json!({"id": waiting_for, "method": "model/list", "params": params})).await?;
        }
        Ok::<_, std::io::Error>(models)
    };
    let listed = tokio::time::timeout(options.timeout, exchange).await;
    drop(stdin);
    if let Some(group) = group {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(group),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    let _ = child.wait().await;
    match listed {
        Ok(Ok(models)) => Ok(models),
        Ok(Err(source)) => Err(spawn(source)),
        Err(_) => Err(spawn(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "codex did not answer `model/list` in time",
        ))),
    }
}
