# M1 · P1 Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Rust workspace whose `harness ask` command completes multi-step coding tasks headlessly against any OpenAI-compatible model (Ollama, LM Studio, llama.cpp, OpenRouter, custom endpoints), with six tools, mode-based permissions, retries, interruption, NDJSON output, and documented exit codes.

**Architecture:** A library core (`harness-core`) owns provider-neutral messages, the `Provider`/`Tool` traits, the permission policy, and the agent loop, which reports everything as a typed `AgentEvent` stream. Leaf crates implement config (`harness-config`), the OpenAI-compatible adapter and discovery (`harness-providers`), and the tools (`harness-tools`). The `harness` binary (`harness-cli`) is a thin frontend that renders the event stream.

**Tech Stack:** Rust 1.98 (edition 2024), tokio, reqwest 0.13 (rustls), eventsource-stream, async-stream, serde/serde_json, toml 1.x, jsonschema 0.57, similar 3.x, ignore/globset/regex, nix, clap 4; tests use tempfile, wiremock, assert_cmd, predicates.

**Spec:** `openspec/changes/add-core-agent/` (proposal.md, design.md, specs/*/spec.md). This plan is phase P1 of five; the phase map is in `openspec/changes/add-core-agent/tasks.md`.

## Global Constraints

- Toolchain pinned to Rust `1.98.0` via `rust-toolchain.toml`; edition 2024; `rust-version = "1.98"`.
- Licence `MIT OR Apache-2.0` on every crate.
- Platforms: macOS and Linux only (Unix APIs such as process groups are fine).
- No telemetry: the harness itself connects only to configured/built-in providers and localhost discovery probes.
- Never print, log, or persist API key values.
- Config/data/state live in XDG directories (`$XDG_CONFIG_HOME/harness`, `$XDG_DATA_HOME/harness`, `$XDG_STATE_HOME/harness`, defaults under `~/.config`, `~/.local/share`, `~/.local/state`); `HARNESS_HOME` overrides all three with `config/`, `data/`, `state/` subdirectories.
- Model ids are `<provider>/<model>`, split at the first `/`.
- Built-in providers in P1: `ollama` (`http://127.0.0.1:11434/v1`), `lmstudio` (`http://127.0.0.1:1234/v1`), `llamacpp` (`http://127.0.0.1:8080/v1`), `openrouter` (`https://openrouter.ai/api/v1`, key in `OPENROUTER_API_KEY`). `openai`, `anthropic`, and `chatgpt` arrive in P4.
- Local discovery probes time out within 300 ms each.
- Tool order is fixed: `read, write, edit, bash, grep, glob`; the six definitions together must not exceed 1,500 tokens (≈6,000 characters of JSON); the base system prompt must not exceed 1,000 tokens.
- Tool output over 10 KB (10,240 bytes) is saved to `$XDG_STATE_HOME/harness/tool-output/<run-id>/<call-id>.txt` and replaced by head + tail + omitted size + path.
- Retries: network errors, HTTP 429, HTTP 5xx only; up to 5 attempts; `Retry-After` (seconds) honoured; never retry after output was already streamed to the user.
- Step limit: 50 model calls per turn by default.
- `harness ask` exit codes: `0` success, `1` runtime error, `2` invalid usage / no usable model / bad config, `3` completed with an action blocked for lack of approval, `130` interrupted.
- No OS sandbox exists in P1, so every `bash` call needs approval outside `full-access` (spec: "No silent unsandboxed fallback"). P2 adds the sandbox.
- Until workspace trust exists (P2), a project `.harness/config.toml` may only set `model`, `max_steps`, and the modes `plan`, `read-only`, `ask`; other settings are ignored with a warning.
- Design deviation (recorded here on purpose): the `Provider` and `Tool` traits live in `harness-core`, not in the leaf crates, to avoid a dependency cycle. `harness-context` and `harness-sandbox` are created in P3 and P2.

## Review Focus

- **Non-canonical or symlinked workspace paths** (macOS temp dirs live under `/var` → `/private/var`; users `cd` through symlinks): in-workspace files must still count as inside. Test in Task 4.
- **Local servers that omit tool-call `id` or `index` fields**, or stream several calls without indexes: each call must still execute exactly once. Test in Task 9.
- **A command that leaves a background process holding stdout open** (`npm run dev &`): `bash` must still return at its timeout and kill the whole group. Test in Task 8.
- **Huge single-line files** (minified JS, lockfiles): `read` must cap each line and say so instead of flooding context. Test in Task 6.
- **Ctrl+C during `harness ask`**: the run stops promptly and exits `130`. Test in Task 13.

---

## File Map

```
Cargo.toml                         workspace + shared dependency versions
rust-toolchain.toml                toolchain pin
deny.toml                          cargo-deny policy
LICENSE-MIT, LICENSE-APACHE        licences
.github/workflows/ci.yml           CI: fmt, clippy, nextest, deny (macOS + Ubuntu)
crates/harness-core/
  src/lib.rs                       module list
  src/message.rs                   Message, ToolCall, ToolSpec, Usage, ChatRequest
  src/provider.rs                  Provider trait, ProviderEvent, ProviderError, FinishReason
  src/event.rs                     AgentEvent, TurnEndReason, ErrorKind
  src/permission.rs                Mode, Action, Decision, resolve_path, PermissionPolicy, BaselinePolicy
  src/tool.rs                      Tool trait, ToolContext, ToolOutput, ReadTracker, ToolRegistry
  src/output.rs                    limit_output (spill large output to a file)
  src/agent.rs                     Agent loop, AgentConfig, Approver, NonInteractive
  src/retry.rs                     RetryPolicy
  src/testing.rs                   MockProvider + Script (test double, used by tests)
  tests/…                          integration tests per area
crates/harness-config/
  src/lib.rs, src/paths.rs, src/config.rs, tests/config.rs
crates/harness-providers/
  src/lib.rs, src/openai_chat.rs, src/discovery.rs, src/registry.rs, tests/…
crates/harness-tools/
  src/lib.rs (builtin()), src/{read,write,edit,bash,grep,glob,walk}.rs, tests/…
crates/harness-cli/
  src/main.rs, src/setup.rs, src/prompt.rs, src/ask.rs, src/models.rs, tests/…
```

---

### Task 1: Workspace scaffold, licences, CI, and `harness --version`

**Files:**
- Create: `Cargo.toml`, `rust-toolchain.toml`, `deny.toml`, `LICENSE-MIT`, `LICENSE-APACHE`, `.github/workflows/ci.yml`
- Create: `crates/harness-cli/Cargo.toml`, `crates/harness-cli/src/main.rs`
- Test: `crates/harness-cli/tests/cli_smoke.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: the workspace (`crates/*` members), `[workspace.dependencies]` versions used by every later task, and a `harness` binary built from package `harness-cli`.

- [ ] **Step 1: Create the workspace files**

`Cargo.toml`:

```toml
[workspace]
resolver = "3"
members = ["crates/*"]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.98"
license = "MIT OR Apache-2.0"

[workspace.dependencies]
anyhow = "1.0.104"
async-stream = "0.3"
async-trait = "0.1.92"
clap = { version = "4.6.7", features = ["derive"] }
eventsource-stream = "0.2.3"
futures = "0.3.34"
globset = "0.4.20"
hex = "0.4.3"
ignore = "0.4.33"
jsonschema = "0.57.0"
nix = { version = "0.31.3", features = ["signal", "process"] }
regex = "1"
reqwest = { version = "0.13.5", default-features = false, features = ["json", "stream", "rustls"] }
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.151"
sha2 = "0.11.0"
similar = "3.2.0"
thiserror = "2.0.21"
tokio = { version = "1.53.1", features = ["full"] }
tokio-util = "0.7.19"
toml = "1.1.6"
assert_cmd = "2.2.2"
predicates = "3.1.4"
tempfile = "3.27.0"
wiremock = "0.6.5"
```

`rust-toolchain.toml`:

```toml
[toolchain]
channel = "1.98.0"
components = ["rustfmt", "clippy"]
```

`deny.toml`:

```toml
[graph]
all-features = true

[advisories]
version = 2
yanked = "deny"

[licenses]
version = 2
allow = [
  "MIT", "Apache-2.0", "Apache-2.0 WITH LLVM-exception", "BSD-2-Clause", "BSD-3-Clause",
  "ISC", "Unicode-3.0", "Zlib", "CDLA-Permissive-2.0", "MPL-2.0", "BSL-1.0",
]
confidence-threshold = 0.9

[bans]
multiple-versions = "warn"
wildcards = "deny"
allow-wildcard-paths = true

[sources]
unknown-registry = "deny"
unknown-git = "deny"
```

`LICENSE-MIT`:

```text
MIT License

Copyright (c) 2026 The harness contributors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

`LICENSE-APACHE`: download the canonical text.

Run: `curl -fsSL https://www.apache.org/licenses/LICENSE-2.0.txt -o LICENSE-APACHE && head -3 LICENSE-APACHE`
Expected: the file starts with `Apache License` / `Version 2.0, January 2004`.

`.github/workflows/ci.yml`:

```yaml
name: CI
on:
  push:
    branches: [master]
  pull_request:

jobs:
  test:
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - run: rustup show
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --all --check
      - run: cargo clippy --workspace --all-targets -- -D warnings
      - uses: taiki-e/install-action@nextest
      - run: cargo nextest run --workspace
  deny:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: EmbarkStudios/cargo-deny-action@v2
```

`crates/harness-cli/Cargo.toml`:

```toml
[package]
name = "harness-cli"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[[bin]]
name = "harness"
path = "src/main.rs"

[dependencies]
clap.workspace = true

[dev-dependencies]
assert_cmd.workspace = true
predicates.workspace = true
```

`crates/harness-cli/src/main.rs` (deliberately empty so the test fails first):

```rust
fn main() {}
```

- [ ] **Step 2: Write the failing test**

`crates/harness-cli/tests/cli_smoke.rs`:

```rust
use assert_cmd::Command;
use predicates::str::contains;

#[test]
fn prints_version() {
    Command::new(env!("CARGO_BIN_EXE_harness"))
        .arg("--version")
        .assert()
        .success()
        .stdout(contains("harness 0.1.0"));
}
```

- [ ] **Step 3: Run it to verify it fails**

Run: `cargo test -p harness-cli --test cli_smoke`
Expected: FAIL: stdout is empty, so `contains("harness 0.1.0")` does not match.

- [ ] **Step 4: Implement the CLI entry point**

`crates/harness-cli/src/main.rs`:

```rust
use clap::Parser;

#[derive(Parser)]
#[command(name = "harness", version, about = "A hybrid local/frontier coding agent")]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
}
```

- [ ] **Step 5: Run the test and the lint gates**

Run: `cargo test -p harness-cli --test cli_smoke && cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: `test prints_version ... ok`; fmt and clippy print nothing and exit 0.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock rust-toolchain.toml deny.toml LICENSE-MIT LICENSE-APACHE .github crates/harness-cli
git commit -m "chore: scaffold Rust workspace, licences, CI, and harness --version"
```

---

### Task 2: Core domain types

**Files:**
- Create: `crates/harness-core/Cargo.toml`, `crates/harness-core/src/lib.rs`, `src/message.rs`, `src/provider.rs`, `src/event.rs`, `src/permission.rs`
- Modify: `Cargo.toml` (add `harness-core` to `[workspace.dependencies]`)
- Test: `crates/harness-core/tests/types.rs`

**Interfaces:**
- Consumes: workspace dependency versions (Task 1).
- Produces:
  - `harness_core::message::{Message, ToolCall, ToolSpec, Usage, ChatRequest}`; `Message` variants `User { content }`, `Assistant { content, tool_calls, model }`, `Tool { call_id, content, is_error }`; `ToolCall { id, name, arguments: String }`; `ChatRequest { model, system, messages, tools }`.
  - `harness_core::provider::{Provider, ProviderStream, ProviderEvent, ProviderError, FinishReason}`; `trait Provider: Send + Sync { fn stream(&self, request: ChatRequest) -> ProviderStream; }`; `ProviderError::{Network(String), Http { status: u16, body: String, retry_after: Option<Duration> }, Protocol(String)}` with `is_retryable()` and `retry_after()`.
  - `harness_core::event::{AgentEvent, TurnEndReason, ErrorKind}` (serde tag `type`, snake_case).
  - `harness_core::permission::{Mode, Action, Decision}`; `Mode::{Plan, ReadOnly, Ask, Auto, FullAccess}` (kebab-case, `FromStr`, `Display`, `is_narrow()`); `Action::{Read(PathBuf), Write(PathBuf), Bash(String)}`; `Decision::{Allow, Ask(String), Deny(String)}`.

- [ ] **Step 1: Create the crate manifest and register it**

Add to root `Cargo.toml` under `[workspace.dependencies]`:

```toml
harness-core = { path = "crates/harness-core" }
```

`crates/harness-core/Cargo.toml` (declares every dependency the core needs in P1):

```toml
[package]
name = "harness-core"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
async-trait.workspace = true
futures.workspace = true
hex.workspace = true
jsonschema.workspace = true
serde.workspace = true
serde_json.workspace = true
sha2.workspace = true
thiserror.workspace = true
tokio.workspace = true
tokio-util.workspace = true

[dev-dependencies]
tempfile.workspace = true
tokio = { workspace = true, features = ["test-util"] }
```

`crates/harness-core/src/lib.rs`:

```rust
//! Core agent runtime for harness: provider-neutral messages, events, permissions, and the agent loop.

pub mod event;
pub mod message;
pub mod permission;
pub mod provider;
```

- [ ] **Step 2: Write the failing tests**

`crates/harness-core/tests/types.rs`:

```rust
use std::time::Duration;

use harness_core::event::{AgentEvent, TurnEndReason};
use harness_core::permission::Mode;
use harness_core::provider::ProviderError;

#[test]
fn events_serialize_as_tagged_objects() {
    let ev = AgentEvent::ToolCallFinished { id: "c1".into(), output: "ok".into(), is_error: false };
    assert_eq!(
        serde_json::to_string(&ev).unwrap(),
        r#"{"type":"tool_call_finished","id":"c1","output":"ok","is_error":false}"#
    );
    let end = AgentEvent::TurnFinished { reason: TurnEndReason::StepLimit };
    assert_eq!(serde_json::to_string(&end).unwrap(), r#"{"type":"turn_finished","reason":"step_limit"}"#);
}

#[test]
fn modes_parse_from_kebab_case() {
    assert_eq!("full-access".parse::<Mode>().unwrap(), Mode::FullAccess);
    assert_eq!("read-only".parse::<Mode>().unwrap(), Mode::ReadOnly);
    assert_eq!(Mode::ReadOnly.to_string(), "read-only");
    assert!("yolo".parse::<Mode>().is_err());
    assert!(Mode::Ask.is_narrow());
    assert!(!Mode::Auto.is_narrow());
}

#[test]
fn only_network_429_and_5xx_are_retryable() {
    let http = |status| ProviderError::Http { status, body: String::new(), retry_after: None };
    assert!(ProviderError::Network("reset".into()).is_retryable());
    assert!(http(429).is_retryable());
    assert!(http(503).is_retryable());
    assert!(!http(401).is_retryable());
    assert!(!ProviderError::Protocol("bad json".into()).is_retryable());
    let limited = ProviderError::Http { status: 429, body: String::new(), retry_after: Some(Duration::from_secs(3)) };
    assert_eq!(limited.retry_after(), Some(Duration::from_secs(3)));
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p harness-core --test types`
Expected: FAIL to compile: modules `event`, `message`, `permission`, and `provider` do not exist yet.

- [ ] **Step 4: Implement the types**

`crates/harness-core/src/message.rs`:

```rust
use serde::{Deserialize, Serialize};

/// A tool invocation requested by the model. `arguments` is the raw JSON text the model produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// A provider-neutral conversation message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User { content: String },
    Assistant { content: String, tool_calls: Vec<ToolCall>, model: String },
    Tool { call_id: String, content: String, is_error: bool },
}

/// A tool definition sent to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Token accounting reported by a provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
}

/// Everything a provider needs for one model call.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// Model name as the provider knows it (without the `<provider>/` prefix).
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
}
```

`crates/harness-core/src/provider.rs`:

```rust
use std::time::Duration;

use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::message::{ChatRequest, ToolCall, Usage};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    Other(String),
}

/// One item of a provider's streamed reply. Tool calls arrive complete, never as fragments.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCall(ToolCall),
    Usage(Usage),
    Finished(FinishReason),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    #[error("network error: {0}")]
    Network(String),
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String, retry_after: Option<Duration> },
    #[error("invalid provider response: {0}")]
    Protocol(String),
}

impl ProviderError {
    /// Network errors, HTTP 429, and HTTP 5xx are worth retrying.
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::Network(_) => true,
            ProviderError::Http { status, .. } => *status == 429 || (500..600).contains(status),
            ProviderError::Protocol(_) => false,
        }
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            ProviderError::Http { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

pub type ProviderStream = BoxStream<'static, Result<ProviderEvent, ProviderError>>;

/// A model backend: translates a [`ChatRequest`] to a wire protocol and streams events back.
pub trait Provider: Send + Sync {
    fn stream(&self, request: ChatRequest) -> ProviderStream;
}
```

`crates/harness-core/src/event.rs`:

```rust
use serde::{Deserialize, Serialize};

use crate::message::Usage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEndReason {
    Completed,
    StepLimit,
    Interrupted,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Provider,
    Internal,
}

/// Everything observable about a turn. Frontends render these; `harness ask --json` prints one per line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    TurnStarted,
    TextDelta { text: String },
    ReasoningDelta { text: String },
    AssistantMessage { content: String, model: String },
    ToolCallRequested { id: String, name: String, arguments: String },
    ApprovalNeeded { id: String, reason: String },
    ActionBlocked { id: String, reason: String },
    ToolCallFinished { id: String, output: String, is_error: bool },
    Usage { model: String, usage: Usage },
    Retrying { attempt: u32, reason: String, delay_ms: u64 },
    TurnFinished { reason: TurnEndReason },
    Error { kind: ErrorKind, message: String },
}
```

`crates/harness-core/src/permission.rs`:

```rust
use std::{fmt, path::PathBuf, str::FromStr};

use serde::{Deserialize, Serialize};

/// Approval mode. See the permissions-sandbox spec for the exact semantics of each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    Plan,
    ReadOnly,
    Ask,
    Auto,
    FullAccess,
}

impl Mode {
    /// Modes that grant no more than `ask` does. Only these may come from an untrusted project config.
    pub fn is_narrow(self) -> bool {
        matches!(self, Mode::Plan | Mode::ReadOnly | Mode::Ask)
    }
}

impl FromStr for Mode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "plan" => Ok(Mode::Plan),
            "read-only" => Ok(Mode::ReadOnly),
            "ask" => Ok(Mode::Ask),
            "auto" => Ok(Mode::Auto),
            "full-access" => Ok(Mode::FullAccess),
            other => Err(format!(
                "unknown mode `{other}` (expected plan, read-only, ask, auto, or full-access)"
            )),
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Mode::Plan => "plan",
            Mode::ReadOnly => "read-only",
            Mode::Ask => "ask",
            Mode::Auto => "auto",
            Mode::FullAccess => "full-access",
        })
    }
}

/// What a tool call is about to do, as seen by the permission policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Read(PathBuf),
    Write(PathBuf),
    Bash(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask(String),
    Deny(String),
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p harness-core --test types && cargo clippy -p harness-core --all-targets -- -D warnings`
Expected: 3 tests pass; clippy is clean.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/harness-core
git commit -m "feat(core): add provider-neutral message, event, provider, and permission types"
```

---

### Task 3: XDG paths and layered configuration

**Files:**
- Create: `crates/harness-config/Cargo.toml`, `src/lib.rs`, `src/paths.rs`, `src/config.rs`
- Modify: `Cargo.toml` (add `harness-config` to `[workspace.dependencies]`)
- Test: `crates/harness-config/tests/config.rs`

**Interfaces:**
- Consumes: `harness_core::permission::Mode` (Task 2).
- Produces:
  - `harness_config::paths::{Paths, PathsError}`; `Paths { config_dir, data_dir, state_dir }`; `Paths::from_env(get: impl Fn(&str) -> Option<String>) -> Result<Paths, PathsError>`; `Paths::from_process_env()`; `Paths::global_config_file() -> PathBuf`.
  - `harness_config::config::{Config, ConfigFile, ConfigError, ProviderConfig, Protocol, load, parse_file}`; `Config { model: Option<String>, mode: Option<Mode>, max_steps: Option<u32>, providers: BTreeMap<String, ProviderConfig>, warnings: Vec<String> }`; `ProviderConfig { protocol: Protocol, base_url: String, api_key_env: Option<String> }`; `Protocol::OpenaiChat` (TOML value `"openai-chat"`); `load(global_file: &Path, workspace: &Path) -> Result<Config, ConfigError>`.

- [ ] **Step 1: Create the crate manifest and register it**

Add to root `Cargo.toml` under `[workspace.dependencies]`:

```toml
harness-config = { path = "crates/harness-config" }
```

`crates/harness-config/Cargo.toml`:

```toml
[package]
name = "harness-config"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
harness-core.workspace = true
serde.workspace = true
thiserror.workspace = true
toml.workspace = true

[dev-dependencies]
tempfile.workspace = true
```

`crates/harness-config/src/lib.rs`:

```rust
//! Where harness keeps its files, and how configuration layers combine.

pub mod config;
pub mod paths;
```

Create empty `src/paths.rs` and `src/config.rs` so the crate compiles.

- [ ] **Step 2: Write the failing tests**

`crates/harness-config/tests/config.rs`:

```rust
use std::{collections::HashMap, path::PathBuf};

use harness_config::{config, paths::Paths};
use harness_core::permission::Mode;

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    move |k| map.get(k).cloned()
}

#[test]
fn defaults_follow_xdg_under_home() {
    let p = Paths::from_env(env(&[("HOME", "/home/u")])).unwrap();
    assert_eq!(p.config_dir, PathBuf::from("/home/u/.config/harness"));
    assert_eq!(p.data_dir, PathBuf::from("/home/u/.local/share/harness"));
    assert_eq!(p.state_dir, PathBuf::from("/home/u/.local/state/harness"));
    assert_eq!(p.global_config_file(), PathBuf::from("/home/u/.config/harness/config.toml"));
}

#[test]
fn xdg_variables_override_defaults_and_relative_values_are_ignored() {
    let p = Paths::from_env(env(&[
        ("HOME", "/home/u"),
        ("XDG_CONFIG_HOME", "/tmp/cfg"),
        ("XDG_DATA_HOME", "relative/data"),
    ]))
    .unwrap();
    assert_eq!(p.config_dir, PathBuf::from("/tmp/cfg/harness"));
    assert_eq!(p.data_dir, PathBuf::from("/home/u/.local/share/harness"));
}

#[test]
fn harness_home_overrides_everything() {
    let p = Paths::from_env(env(&[("HOME", "/home/u"), ("HARNESS_HOME", "/opt/h"), ("XDG_CONFIG_HOME", "/tmp/cfg")]))
        .unwrap();
    assert_eq!(p.config_dir, PathBuf::from("/opt/h/config"));
    assert_eq!(p.data_dir, PathBuf::from("/opt/h/data"));
    assert_eq!(p.state_dir, PathBuf::from("/opt/h/state"));
}

#[test]
fn missing_home_is_an_error() {
    assert!(Paths::from_env(env(&[])).is_err());
}

#[test]
fn unknown_keys_report_file_and_line() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    std::fs::write(&file, "model = \"ollama/a\"\nmdoe = \"auto\"\n").unwrap();
    let err = config::load(&file, dir.path()).unwrap_err().to_string();
    assert!(err.contains("config.toml"), "{err}");
    assert!(err.contains("line 2"), "{err}");
    assert!(err.contains("mdoe"), "{err}");
}

#[test]
fn missing_files_yield_an_empty_config() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config::load(&dir.path().join("nope.toml"), dir.path()).unwrap();
    assert_eq!(cfg, config::Config::default());
}

#[test]
fn project_config_overrides_model_but_cannot_widen() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global.toml");
    std::fs::write(
        &global,
        "model = \"ollama/a\"\nmode = \"auto\"\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\n",
    )
    .unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(ws.join(".harness")).unwrap();
    std::fs::write(
        ws.join(".harness/config.toml"),
        "model = \"ollama/b\"\nmode = \"full-access\"\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"http://evil.example/v1\"\n",
    )
    .unwrap();

    let cfg = config::load(&global, &ws).unwrap();
    assert_eq!(cfg.model.as_deref(), Some("ollama/b"));
    assert_eq!(cfg.mode, Some(Mode::Auto));
    assert_eq!(cfg.providers["mock"].base_url, "http://127.0.0.1:9/v1");
    assert_eq!(cfg.warnings.len(), 2, "{:?}", cfg.warnings);
}

#[test]
fn project_config_may_narrow_the_mode() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global.toml");
    std::fs::write(&global, "mode = \"auto\"\n").unwrap();
    std::fs::create_dir_all(dir.path().join(".harness")).unwrap();
    std::fs::write(dir.path().join(".harness/config.toml"), "mode = \"ask\"\n").unwrap();
    let cfg = config::load(&global, dir.path()).unwrap();
    assert_eq!(cfg.mode, Some(Mode::Ask));
    assert!(cfg.warnings.is_empty());
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p harness-config --test config`
Expected: FAIL to compile: `Paths` and `config::load` are not defined.

- [ ] **Step 4: Implement paths and config**

`crates/harness-config/src/paths.rs`:

```rust
use std::path::PathBuf;

/// The three directories harness uses, per the XDG base-directory spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub state_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathsError {
    #[error("cannot determine the home directory: HOME is not set")]
    NoHome,
}

impl Paths {
    /// Resolves directories from environment variables. `HARNESS_HOME` overrides everything;
    /// relative XDG values are ignored, as the XDG spec requires.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Paths, PathsError> {
        if let Some(root) = get("HARNESS_HOME").filter(|v| !v.is_empty()) {
            let root = PathBuf::from(root);
            return Ok(Paths {
                config_dir: root.join("config"),
                data_dir: root.join("data"),
                state_dir: root.join("state"),
            });
        }
        let home = get("HOME").filter(|v| !v.is_empty()).map(PathBuf::from);
        let base = |var: &str, fallback: &str| -> Result<PathBuf, PathsError> {
            match get(var).map(PathBuf::from).filter(|p| p.is_absolute()) {
                Some(dir) => Ok(dir.join("harness")),
                None => Ok(home.clone().ok_or(PathsError::NoHome)?.join(fallback).join("harness")),
            }
        };
        Ok(Paths {
            config_dir: base("XDG_CONFIG_HOME", ".config")?,
            data_dir: base("XDG_DATA_HOME", ".local/share")?,
            state_dir: base("XDG_STATE_HOME", ".local/state")?,
        })
    }

    pub fn from_process_env() -> Result<Paths, PathsError> {
        Self::from_env(|key| std::env::var(key).ok())
    }

    pub fn global_config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
}
```

`crates/harness-config/src/config.rs`:

```rust
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use harness_core::permission::Mode;
use serde::Deserialize;

/// Wire protocol spoken by a configured provider. P4 adds `openai-responses` and `anthropic-messages`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    OpenaiChat,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub protocol: Protocol,
    pub base_url: String,
    pub api_key_env: Option<String>,
}

/// One `config.toml` file as written by the user.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    pub model: Option<String>,
    pub mode: Option<Mode>,
    pub max_steps: Option<u32>,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("invalid config {path}: {message}")]
    Parse { path: PathBuf, message: String },
}

/// The merged, effective configuration plus warnings about settings that were ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub model: Option<String>,
    pub mode: Option<Mode>,
    pub max_steps: Option<u32>,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub warnings: Vec<String>,
}

/// Parses one config file. A missing file is `Ok(None)`; an invalid one is an error naming file and line.
pub fn parse_file(path: &Path) -> Result<Option<ConfigFile>, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(ConfigError::Io { path: path.to_path_buf(), source }),
    };
    toml::from_str(&text)
        .map(Some)
        .map_err(|e| ConfigError::Parse { path: path.to_path_buf(), message: e.to_string() })
}

/// Loads the global config, then the workspace's `.harness/config.toml`.
///
/// Until workspace trust exists (P2), the project file may only set `model`, `max_steps`, and narrowing
/// modes; widening settings are ignored with a warning.
pub fn load(global_file: &Path, workspace: &Path) -> Result<Config, ConfigError> {
    let mut cfg = Config::default();
    if let Some(global) = parse_file(global_file)? {
        cfg.model = global.model;
        cfg.mode = global.mode;
        cfg.max_steps = global.max_steps;
        cfg.providers = global.providers;
    }
    let project_file = workspace.join(".harness").join("config.toml");
    if let Some(project) = parse_file(&project_file)? {
        if project.model.is_some() {
            cfg.model = project.model;
        }
        if project.max_steps.is_some() {
            cfg.max_steps = project.max_steps;
        }
        match project.mode {
            Some(mode) if mode.is_narrow() => cfg.mode = Some(mode),
            Some(mode) => cfg.warnings.push(format!(
                "{}: ignoring mode `{mode}` from untrusted project config",
                project_file.display()
            )),
            None => {}
        }
        if !project.providers.is_empty() {
            cfg.warnings.push(format!(
                "{}: ignoring provider definitions from untrusted project config",
                project_file.display()
            ));
        }
    }
    Ok(cfg)
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p harness-config --test config && cargo clippy -p harness-config --all-targets -- -D warnings`
Expected: 8 tests pass; clippy clean.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/harness-config
git commit -m "feat(config): add XDG paths and layered config that ignores widening project settings"
```

---

### Task 4: Path resolution and the baseline permission policy

**Files:**
- Modify: `crates/harness-core/src/permission.rs` (append)
- Test: `crates/harness-core/tests/permission.rs`

**Interfaces:**
- Consumes: `Mode`, `Action`, `Decision` (Task 2).
- Produces:
  - `harness_core::permission::resolve_path(workspace: &Path, path: &Path) -> PathBuf`: absolute path with symlinks resolved component by component (so `link/..` follows the real target).
  - `trait PermissionPolicy: Send + Sync { fn check(&self, action: &Action) -> Decision; }`
  - `BaselinePolicy::new(mode: Mode, workspace: &Path, read_dirs: Vec<PathBuf>) -> BaselinePolicy` and `BaselinePolicy::mode()`. Rules: `full-access` allows everything; reads inside the workspace or `read_dirs` are allowed, others ask; writes are denied in `plan`/`read-only`, ask outside the workspace, are allowed in `auto`, and ask in `ask`; `bash` always asks (no sandbox until P2).

- [ ] **Step 1: Write the failing tests**

`crates/harness-core/tests/permission.rs`:

```rust
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use harness_core::permission::{resolve_path, Action, BaselinePolicy, Decision, Mode, PermissionPolicy};

fn is_ask(d: Decision) -> bool {
    matches!(d, Decision::Ask(_))
}

fn write(p: &str) -> Action {
    Action::Write(PathBuf::from(p))
}

#[test]
fn auto_allows_writes_inside_the_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let policy = BaselinePolicy::new(Mode::Auto, dir.path(), vec![]);
    assert_eq!(policy.check(&write("src/new.rs")), Decision::Allow);
}

#[test]
fn plan_and_read_only_reject_writes() {
    let dir = tempfile::tempdir().unwrap();
    for mode in [Mode::Plan, Mode::ReadOnly] {
        let policy = BaselinePolicy::new(mode, dir.path(), vec![]);
        assert!(matches!(policy.check(&write("a.txt")), Decision::Deny(_)), "{mode}");
    }
}

#[test]
fn ask_mode_asks_for_writes() {
    let dir = tempfile::tempdir().unwrap();
    let policy = BaselinePolicy::new(Mode::Ask, dir.path(), vec![]);
    assert!(is_ask(policy.check(&write("a.txt"))));
}

#[test]
fn writes_outside_the_workspace_ask_even_in_auto() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let policy = BaselinePolicy::new(Mode::Auto, &ws, vec![]);
    assert!(is_ask(policy.check(&write("../outside.txt"))));
    assert!(is_ask(policy.check(&write("/etc/hosts"))));
}

#[test]
fn symlink_escapes_are_detected() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let outside = dir.path().join("outside");
    std::fs::create_dir(&ws).unwrap();
    std::fs::create_dir(&outside).unwrap();
    symlink(&outside, ws.join("link")).unwrap();
    let policy = BaselinePolicy::new(Mode::Auto, &ws, vec![]);
    assert!(is_ask(policy.check(&write("link/file.txt"))));
    // `link/..` is the parent of the real target, i.e. outside the workspace.
    assert!(is_ask(policy.check(&write("link/../escape.txt"))));
}

// Review Focus: non-canonical or symlinked workspace paths.
#[test]
fn a_symlinked_workspace_path_still_counts_as_inside() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let alias = dir.path().join("alias");
    symlink(&real, &alias).unwrap();
    let policy = BaselinePolicy::new(Mode::Auto, &alias, vec![]);
    assert_eq!(policy.check(&write("a.txt")), Decision::Allow);
    assert_eq!(policy.check(&Action::Write(real.join("b.txt"))), Decision::Allow);
    assert_eq!(policy.check(&Action::Write(alias.join("c.txt"))), Decision::Allow);
}

#[test]
fn reads_outside_ask_except_allowed_read_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let spill = dir.path().join("spill");
    std::fs::create_dir(&ws).unwrap();
    std::fs::create_dir(&spill).unwrap();
    let policy = BaselinePolicy::new(Mode::Auto, &ws, vec![spill.clone()]);
    assert_eq!(policy.check(&Action::Read(PathBuf::from("README.md"))), Decision::Allow);
    assert_eq!(policy.check(&Action::Read(spill.join("call_1.txt"))), Decision::Allow);
    assert!(is_ask(policy.check(&Action::Read(dir.path().join("secret")))));
}

#[test]
fn bash_asks_without_a_sandbox_and_full_access_allows_everything() {
    let dir = tempfile::tempdir().unwrap();
    let auto = BaselinePolicy::new(Mode::Auto, dir.path(), vec![]);
    assert!(is_ask(auto.check(&Action::Bash("cargo test".into()))));
    let full = BaselinePolicy::new(Mode::FullAccess, dir.path(), vec![]);
    assert_eq!(full.check(&Action::Bash("rm -rf target".into())), Decision::Allow);
    assert_eq!(full.check(&write("/tmp/x")), Decision::Allow);
}

#[test]
fn resolve_path_handles_missing_tails() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    assert_eq!(resolve_path(&ws, Path::new("a/b/../c.txt")), ws.join("a/c.txt"));
    assert_eq!(resolve_path(&ws, Path::new("./x")), ws.join("x"));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p harness-core --test permission`
Expected: FAIL to compile: `resolve_path`, `BaselinePolicy`, and `PermissionPolicy` are not defined.

- [ ] **Step 3: Implement resolution and the policy**

Change the first `use` line of `crates/harness-core/src/permission.rs` to:

```rust
use std::{
    fmt,
    path::{Component, Path, PathBuf},
    str::FromStr,
};
```

Append to the same file:

```rust
/// Resolves `path` (absolute, or relative to `workspace`) to an absolute path. Each existing prefix is
/// canonicalized before the next component is applied, so symlinks are followed exactly as the OS would
/// follow them (including `link/..`). Components that do not exist yet are applied lexically.
pub fn resolve_path(workspace: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() { path.to_path_buf() } else { workspace.join(path) };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if let Ok(real) = out.canonicalize() {
                    out = real;
                }
                out.pop();
            }
            Component::Normal(name) => {
                out.push(name);
                if let Ok(real) = out.canonicalize() {
                    out = real;
                }
            }
        }
    }
    out
}

/// Decides whether an action may run, must be approved, or is refused.
pub trait PermissionPolicy: Send + Sync {
    fn check(&self, action: &Action) -> Decision;
}

/// The P1 policy: mode-based decisions with no OS sandbox. Because no sandbox exists yet, every shell
/// command needs approval outside `full-access` (no silent unsandboxed fallback). P2 replaces this.
#[derive(Debug, Clone)]
pub struct BaselinePolicy {
    mode: Mode,
    workspace: PathBuf,
    read_dirs: Vec<PathBuf>,
}

impl BaselinePolicy {
    pub fn new(mode: Mode, workspace: &Path, read_dirs: Vec<PathBuf>) -> Self {
        let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        BaselinePolicy {
            mode,
            workspace: canonical(workspace),
            read_dirs: read_dirs.iter().map(|d| canonical(d)).collect(),
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }
}

impl PermissionPolicy for BaselinePolicy {
    fn check(&self, action: &Action) -> Decision {
        if self.mode == Mode::FullAccess {
            return Decision::Allow;
        }
        match action {
            Action::Read(path) => {
                let target = resolve_path(&self.workspace, path);
                if target.starts_with(&self.workspace) || self.read_dirs.iter().any(|d| target.starts_with(d)) {
                    Decision::Allow
                } else {
                    Decision::Ask(format!("read outside the workspace: {}", target.display()))
                }
            }
            Action::Write(path) => {
                if matches!(self.mode, Mode::Plan | Mode::ReadOnly) {
                    return Decision::Deny(format!("file writes are not allowed in {} mode", self.mode));
                }
                let target = resolve_path(&self.workspace, path);
                if !target.starts_with(&self.workspace) {
                    Decision::Ask(format!("write outside the workspace: {}", target.display()))
                } else if self.mode == Mode::Auto {
                    Decision::Allow
                } else {
                    Decision::Ask(format!("write {}", target.display()))
                }
            }
            Action::Bash(command) => Decision::Ask(format!("run `{command}` (no sandbox is available yet)")),
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p harness-core --test permission && cargo clippy -p harness-core --all-targets -- -D warnings`
Expected: 9 tests pass; clippy clean.

- [ ] **Step 5: Commit**

```bash
git add crates/harness-core
git commit -m "feat(core): add symlink-aware path resolution and baseline permission policy"
```

---

### Task 5: Tool trait, tool context, read tracking, and output limiting

**Files:**
- Create: `crates/harness-core/src/tool.rs`, `crates/harness-core/src/output.rs`
- Modify: `crates/harness-core/src/lib.rs`
- Test: `crates/harness-core/tests/tool.rs`

**Interfaces:**
- Consumes: `ToolSpec` (Task 2), `Action`, `resolve_path` (Task 4).
- Produces:
  - `harness_core::tool::ToolOutput { content: String, is_error: bool }` with `ToolOutput::ok(..)` / `ToolOutput::error(..)`.
  - `harness_core::tool::ReadTracker` with `record(&self, path: &Path, bytes: &[u8])` and `check_fresh(&self, path: &Path, current: &[u8]) -> Result<(), String>`.
  - `harness_core::tool::ToolContext { workspace: PathBuf (canonical), tracker: Arc<ReadTracker>, cancel: CancellationToken }` with `ToolContext::new(workspace: &Path)` and `resolve(&self, p: &str) -> PathBuf`.
  - `#[async_trait] trait Tool: Send + Sync { fn spec(&self) -> ToolSpec; fn action(&self, args: &Value, ctx: &ToolContext) -> Action; async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput; }`
  - `harness_core::tool::ToolRegistry::new(Vec<Arc<dyn Tool>>)`, `specs() -> Vec<ToolSpec>` (insertion order), `get(&str) -> Option<Arc<dyn Tool>>`.
  - `harness_core::output::{limit_output, DEFAULT_OUTPUT_LIMIT}`; `limit_output(content: &str, limit: usize, dir: &Path, call_id: &str) -> String`.

- [ ] **Step 1: Register the modules**

`crates/harness-core/src/lib.rs` becomes:

```rust
//! Core agent runtime for harness: provider-neutral messages, events, permissions, and the agent loop.

pub mod event;
pub mod message;
pub mod output;
pub mod permission;
pub mod provider;
pub mod tool;
```

Create empty `src/tool.rs` and `src/output.rs`.

- [ ] **Step 2: Write the failing tests**

`crates/harness-core/tests/tool.rs`:

```rust
use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use harness_core::message::ToolSpec;
use harness_core::output::{limit_output, DEFAULT_OUTPUT_LIMIT};
use harness_core::permission::Action;
use harness_core::tool::{ReadTracker, Tool, ToolContext, ToolOutput, ToolRegistry};
use serde_json::{json, Value};

struct Named(&'static str);

#[async_trait]
impl Tool for Named {
    fn spec(&self) -> ToolSpec {
        ToolSpec { name: self.0.into(), description: String::new(), parameters: json!({"type": "object"}) }
    }
    fn action(&self, _args: &Value, ctx: &ToolContext) -> Action {
        Action::Read(ctx.workspace.clone())
    }
    async fn run(&self, _args: Value, _ctx: &ToolContext) -> ToolOutput {
        ToolOutput::ok(self.0)
    }
}

#[test]
fn registry_keeps_insertion_order() {
    let reg = ToolRegistry::new(vec![Arc::new(Named("b")), Arc::new(Named("a")), Arc::new(Named("c"))]);
    let names: Vec<String> = reg.specs().into_iter().map(|s| s.name).collect();
    assert_eq!(names, ["b", "a", "c"]);
    assert!(reg.get("a").is_some());
    assert!(reg.get("zzz").is_none());
}

#[test]
fn tracker_requires_a_prior_unchanged_read() {
    let tracker = ReadTracker::default();
    let path = Path::new("/w/a.txt");
    assert!(tracker.check_fresh(path, b"v1").unwrap_err().contains("read it first"));
    tracker.record(path, b"v1");
    assert!(tracker.check_fresh(path, b"v1").is_ok());
    assert!(tracker.check_fresh(path, b"v2").unwrap_err().contains("changed on disk"));
}

#[test]
fn context_canonicalizes_the_workspace_and_resolves_relative_paths() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext::new(dir.path());
    assert_eq!(ctx.workspace, dir.path().canonicalize().unwrap());
    assert_eq!(ctx.resolve("src/lib.rs"), ctx.workspace.join("src/lib.rs"));
}

#[test]
fn small_output_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(limit_output("hello", DEFAULT_OUTPUT_LIMIT, dir.path(), "c1"), "hello");
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
}

#[test]
fn large_output_is_spilled_to_a_file_with_head_and_tail() {
    let dir = tempfile::tempdir().unwrap();
    let content: String = (1..=5000).map(|i| format!("line {i}\n")).collect();
    let limited = limit_output(&content, 1000, dir.path(), "call/7");
    assert!(limited.starts_with("line 1\n"));
    assert!(limited.trim_end().ends_with("line 5000"));
    assert!(limited.contains("omitted"));
    let saved = dir.path().join("call_7.txt");
    assert!(limited.contains(&saved.display().to_string()), "{limited}");
    assert_eq!(std::fs::read_to_string(saved).unwrap(), content);
    assert!(limited.len() < 1300);
}

#[test]
fn limiting_never_splits_a_multibyte_character() {
    let dir = tempfile::tempdir().unwrap();
    let content = "é".repeat(5000);
    let limited = limit_output(&content, 999, dir.path(), "u");
    assert!(limited.contains("omitted"));
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p harness-core --test tool`
Expected: FAIL to compile: `Tool`, `ToolContext`, `ReadTracker`, `ToolRegistry`, and `limit_output` are not defined.

- [ ] **Step 4: Implement the tool module**

`crates/harness-core/src/tool.rs`:

```rust
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::{
    message::ToolSpec,
    permission::{resolve_path, Action},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        ToolOutput { content: content.into(), is_error: false }
    }

    pub fn error(content: impl Into<String>) -> Self {
        ToolOutput { content: content.into(), is_error: true }
    }
}

/// Remembers the content hash of each file the model has read, so writes can detect stale views.
#[derive(Debug, Default)]
pub struct ReadTracker {
    hashes: Mutex<HashMap<PathBuf, String>>,
}

impl ReadTracker {
    fn hash(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    pub fn record(&self, path: &Path, bytes: &[u8]) {
        self.hashes.lock().expect("tracker lock").insert(path.to_path_buf(), Self::hash(bytes));
    }

    /// `Ok` if the file was read in this session and is unchanged on disk since.
    pub fn check_fresh(&self, path: &Path, current: &[u8]) -> Result<(), String> {
        match self.hashes.lock().expect("tracker lock").get(path) {
            None => Err(format!("{} exists but has not been read in this session; read it first", path.display())),
            Some(hash) if *hash != Self::hash(current) => {
                Err(format!("{} changed on disk since it was read; read it again", path.display()))
            }
            Some(_) => Ok(()),
        }
    }
}

/// What every tool call can see: the workspace, read history, and the turn's cancellation token.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub workspace: PathBuf,
    pub tracker: Arc<ReadTracker>,
    pub cancel: CancellationToken,
}

impl ToolContext {
    pub fn new(workspace: &Path) -> Self {
        ToolContext {
            workspace: workspace.canonicalize().unwrap_or_else(|_| workspace.to_path_buf()),
            tracker: Arc::default(),
            cancel: CancellationToken::new(),
        }
    }

    /// Resolves a path argument against the workspace, following symlinks.
    pub fn resolve(&self, path: &str) -> PathBuf {
        resolve_path(&self.workspace, Path::new(path))
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    /// What running this call would do, for the permission check. Called after schema validation.
    fn action(&self, args: &Value, ctx: &ToolContext) -> Action;
    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput;
}

/// Tools in a fixed order, so tool definitions are byte-identical across requests.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        ToolRegistry { tools }
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.spec().name == name).cloned()
    }
}
```

`crates/harness-core/src/output.rs`:

```rust
use std::path::Path;

/// Tool output above this many bytes is saved to a file instead of being sent whole.
pub const DEFAULT_OUTPUT_LIMIT: usize = 10 * 1024;

/// Caps tool output at roughly `limit` bytes. Larger output is saved in full to `dir/<call_id>.txt`; the
/// model receives the head, the tail, the omitted size, and the file path.
pub fn limit_output(content: &str, limit: usize, dir: &Path, call_id: &str) -> String {
    if content.len() <= limit {
        return content.to_string();
    }
    let safe_id: String =
        call_id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    let file = dir.join(format!("{safe_id}.txt"));
    let saved = std::fs::create_dir_all(dir).and_then(|_| std::fs::write(&file, content));

    let keep = limit * 2 / 5;
    let head_end = floor_boundary(content, keep);
    let tail_start = ceil_boundary(content, content.len() - keep);
    let omitted = &content[head_end..tail_start];
    let location = match saved {
        Ok(()) => format!("full output saved to {}", file.display()),
        Err(e) => format!("full output could not be saved: {e}"),
    };
    format!(
        "{}\n[... {} bytes / {} lines omitted; {location} ...]\n{}",
        &content[..head_end],
        omitted.len(),
        omitted.lines().count(),
        &content[tail_start..]
    )
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p harness-core --test tool && cargo clippy -p harness-core --all-targets -- -D warnings`
Expected: 6 tests pass; clippy clean.

- [ ] **Step 6: Commit**

```bash
git add crates/harness-core
git commit -m "feat(core): add Tool trait, context, read tracking, and large-output spilling"
```

---

### Task 6: `read`, `write`, and `edit` tools

**Files:**
- Create: `crates/harness-tools/Cargo.toml`, `src/lib.rs`, `src/read.rs`, `src/write.rs`, `src/edit.rs`
- Modify: `Cargo.toml` (add `harness-tools` to `[workspace.dependencies]`)
- Test: `crates/harness-tools/tests/fs_tools.rs`

**Interfaces:**
- Consumes: `Tool`, `ToolContext`, `ToolOutput`, `ReadTracker` (Task 5); `Action` (Task 2).
- Produces: `harness_tools::{ReadTool, WriteTool, EditTool}` (unit structs implementing `Tool`, named `read`, `write`, `edit`) and `harness_tools::read::is_binary(&[u8]) -> bool`.

- [ ] **Step 1: Create the crate manifest and register it**

Add to root `Cargo.toml` under `[workspace.dependencies]`:

```toml
harness-tools = { path = "crates/harness-tools" }
```

`crates/harness-tools/Cargo.toml` (declares everything Tasks 6–8 need):

```toml
[package]
name = "harness-tools"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
async-trait.workspace = true
globset.workspace = true
harness-core.workspace = true
ignore.workspace = true
nix.workspace = true
regex.workspace = true
serde_json.workspace = true
similar.workspace = true
tokio.workspace = true

[dev-dependencies]
tempfile.workspace = true
tokio.workspace = true
```

`crates/harness-tools/src/lib.rs`:

```rust
//! The built-in tools: read, write, edit, bash, grep, glob.

pub mod edit;
pub mod read;
pub mod write;

pub use edit::EditTool;
pub use read::ReadTool;
pub use write::WriteTool;
```

Create empty `src/read.rs`, `src/write.rs`, `src/edit.rs`.

- [ ] **Step 2: Write the failing tests**

`crates/harness-tools/tests/fs_tools.rs`:

```rust
use std::path::Path;

use harness_core::tool::{Tool, ToolContext, ToolOutput};
use harness_tools::{EditTool, ReadTool, WriteTool};
use serde_json::{json, Value};

async fn call(tool: &dyn Tool, ctx: &ToolContext, args: Value) -> ToolOutput {
    tool.run(args, ctx).await
}

fn setup() -> (tempfile::TempDir, ToolContext) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext::new(dir.path());
    (dir, ctx)
}

fn put(dir: &Path, name: &str, content: &str) {
    std::fs::write(dir.join(name), content).unwrap();
}

#[tokio::test]
async fn read_returns_numbered_lines_with_offset_and_limit() {
    let (dir, ctx) = setup();
    let text: String = (1..=200).map(|i| format!("line {i}\n")).collect();
    put(dir.path(), "a.txt", &text);
    let out = call(&ReadTool, &ctx, json!({"path": "a.txt", "offset": 100, "limit": 50})).await;
    assert!(!out.is_error);
    assert!(out.content.starts_with("   100\tline 100\n"), "{}", out.content);
    assert!(out.content.contains("   149\tline 149\n"));
    assert!(!out.content.contains("line 150\n"));
    assert!(out.content.contains("offset=150"));
}

#[tokio::test]
async fn read_refuses_binary_files() {
    let (dir, ctx) = setup();
    std::fs::write(dir.path().join("img.png"), [0x89, b'P', b'N', b'G', 0, 0, 1]).unwrap();
    let out = call(&ReadTool, &ctx, json!({"path": "img.png"})).await;
    assert!(out.is_error);
    assert!(out.content.contains("binary"));
}

// Review Focus: huge single-line files.
#[tokio::test]
async fn read_caps_very_long_lines() {
    let (dir, ctx) = setup();
    put(dir.path(), "min.js", &"x".repeat(10_000));
    let out = call(&ReadTool, &ctx, json!({"path": "min.js"})).await;
    assert!(!out.is_error);
    assert!(out.content.contains("[line truncated]"));
    assert!(out.content.len() < 2_200, "{}", out.content.len());
}

#[tokio::test]
async fn read_reports_a_missing_file() {
    let (_dir, ctx) = setup();
    let out = call(&ReadTool, &ctx, json!({"path": "nope.txt"})).await;
    assert!(out.is_error);
    assert!(out.content.contains("nope.txt"));
}

#[tokio::test]
async fn write_creates_new_files_and_parent_directories() {
    let (dir, ctx) = setup();
    let out = call(&WriteTool, &ctx, json!({"path": "src/deep/new.rs", "content": "fn main() {}\n"})).await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(std::fs::read_to_string(dir.path().join("src/deep/new.rs")).unwrap(), "fn main() {}\n");
}

#[tokio::test]
async fn write_refuses_to_overwrite_an_unread_file() {
    let (dir, ctx) = setup();
    put(dir.path(), "a.txt", "original");
    let out = call(&WriteTool, &ctx, json!({"path": "a.txt", "content": "new"})).await;
    assert!(out.is_error);
    assert!(out.content.contains("read it first"));
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "original");
}

#[tokio::test]
async fn write_refuses_a_file_changed_since_it_was_read() {
    let (dir, ctx) = setup();
    put(dir.path(), "a.txt", "v1");
    call(&ReadTool, &ctx, json!({"path": "a.txt"})).await;
    put(dir.path(), "a.txt", "edited in an editor");
    let out = call(&WriteTool, &ctx, json!({"path": "a.txt", "content": "v2"})).await;
    assert!(out.is_error);
    assert!(out.content.contains("changed on disk"));
}

#[tokio::test]
async fn write_after_read_succeeds_and_allows_a_second_write() {
    let (dir, ctx) = setup();
    put(dir.path(), "a.txt", "v1");
    call(&ReadTool, &ctx, json!({"path": "a.txt"})).await;
    assert!(!call(&WriteTool, &ctx, json!({"path": "a.txt", "content": "v2"})).await.is_error);
    assert!(!call(&WriteTool, &ctx, json!({"path": "a.txt", "content": "v3"})).await.is_error);
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "v3");
}

#[tokio::test]
async fn edit_replaces_a_unique_match_and_returns_a_diff() {
    let (dir, ctx) = setup();
    put(dir.path(), "a.rs", "let x = old();\nlet y = 2;\n");
    call(&ReadTool, &ctx, json!({"path": "a.rs"})).await;
    let out = call(&EditTool, &ctx, json!({"path": "a.rs", "old_string": "old()", "new_string": "new()"})).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("-let x = old();"));
    assert!(out.content.contains("+let x = new();"));
    assert_eq!(std::fs::read_to_string(dir.path().join("a.rs")).unwrap(), "let x = new();\nlet y = 2;\n");
}

#[tokio::test]
async fn edit_rejects_ambiguous_and_missing_matches() {
    let (dir, ctx) = setup();
    put(dir.path(), "a.txt", "x\nx\nx\n");
    call(&ReadTool, &ctx, json!({"path": "a.txt"})).await;
    let ambiguous = call(&EditTool, &ctx, json!({"path": "a.txt", "old_string": "x", "new_string": "y"})).await;
    assert!(ambiguous.is_error);
    assert!(ambiguous.content.contains("matches 3 times"), "{}", ambiguous.content);
    let missing = call(&EditTool, &ctx, json!({"path": "a.txt", "old_string": "zzz", "new_string": "y"})).await;
    assert!(missing.is_error);
    assert!(missing.content.contains("0 matches"));
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "x\nx\nx\n");
}

#[tokio::test]
async fn edit_replace_all_changes_every_match() {
    let (dir, ctx) = setup();
    put(dir.path(), "a.txt", "x\nx\n");
    call(&ReadTool, &ctx, json!({"path": "a.txt"})).await;
    let out =
        call(&EditTool, &ctx, json!({"path": "a.txt", "old_string": "x", "new_string": "y", "replace_all": true})).await;
    assert!(!out.is_error);
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "y\ny\n");
}

#[tokio::test]
async fn edit_requires_a_prior_read() {
    let (dir, ctx) = setup();
    put(dir.path(), "a.txt", "x");
    let out = call(&EditTool, &ctx, json!({"path": "a.txt", "old_string": "x", "new_string": "y"})).await;
    assert!(out.is_error);
    assert!(out.content.contains("read it first"));
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p harness-tools --test fs_tools`
Expected: FAIL to compile: `ReadTool`, `WriteTool`, `EditTool` are not defined.

- [ ] **Step 4: Implement the three tools**

`crates/harness-tools/src/read.rs`:

```rust
use async_trait::async_trait;
use harness_core::{
    message::ToolSpec,
    permission::Action,
    tool::{Tool, ToolContext, ToolOutput},
};
use serde_json::{json, Value};

const DEFAULT_LIMIT: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;

pub struct ReadTool;

/// A file is treated as binary when its first 8 KB contain a NUL byte.
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0)
}

#[async_trait]
impl Tool for ReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read".into(),
            description: "Read a text file. Returns numbered lines; use offset (1-based) and limit to page through large files.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative to the workspace or absolute"},
                    "offset": {"type": "integer", "minimum": 1},
                    "limit": {"type": "integer", "minimum": 1}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    fn action(&self, args: &Value, ctx: &ToolContext) -> Action {
        Action::Read(ctx.resolve(args["path"].as_str().unwrap_or_default()))
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let path = ctx.resolve(args["path"].as_str().unwrap_or_default());
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(e) => return ToolOutput::error(format!("cannot read {}: {e}", path.display())),
        };
        if is_binary(&bytes) {
            return ToolOutput::error(format!("{} is a binary file", path.display()));
        }
        ctx.tracker.record(&path, &bytes);

        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = args["limit"].as_u64().map(|l| l as usize).unwrap_or(DEFAULT_LIMIT);
        if lines.is_empty() {
            return ToolOutput::ok("[empty file]\n");
        }
        let mut out = String::new();
        for (index, line) in lines.iter().enumerate().skip(offset - 1).take(limit) {
            if line.chars().count() > MAX_LINE_CHARS {
                let shown: String = line.chars().take(MAX_LINE_CHARS).collect();
                out.push_str(&format!("{:>6}\t{shown} [line truncated]\n", index + 1));
            } else {
                out.push_str(&format!("{:>6}\t{line}\n", index + 1));
            }
        }
        let end = (offset - 1 + limit).min(lines.len());
        if end < lines.len() {
            out.push_str(&format!(
                "[... {} more lines; call read with offset={} to continue]\n",
                lines.len() - end,
                end + 1
            ));
        }
        if out.is_empty() {
            out = format!("[offset {offset} is past the end of the file ({} lines)]\n", lines.len());
        }
        ToolOutput::ok(out)
    }
}
```

`crates/harness-tools/src/write.rs`:

```rust
use async_trait::async_trait;
use harness_core::{
    message::ToolSpec,
    permission::Action,
    tool::{Tool, ToolContext, ToolOutput},
};
use serde_json::{json, Value};

pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write".into(),
            description: "Create a file or replace its whole content. Read an existing file before overwriting it.".into(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "content": {"type": "string"}},
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        }
    }

    fn action(&self, args: &Value, ctx: &ToolContext) -> Action {
        Action::Write(ctx.resolve(args["path"].as_str().unwrap_or_default()))
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let path = ctx.resolve(args["path"].as_str().unwrap_or_default());
        let content = args["content"].as_str().unwrap_or_default();
        if let Ok(existing) = tokio::fs::read(&path).await {
            if let Err(message) = ctx.tracker.check_fresh(&path, &existing) {
                return ToolOutput::error(message);
            }
        }
        if let Some(parent) = path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return ToolOutput::error(format!("cannot create {}: {e}", parent.display()));
            }
        }
        if let Err(e) = tokio::fs::write(&path, content).await {
            return ToolOutput::error(format!("cannot write {}: {e}", path.display()));
        }
        ctx.tracker.record(&path, content.as_bytes());
        ToolOutput::ok(format!("Wrote {} bytes to {}", content.len(), path.display()))
    }
}
```

`crates/harness-tools/src/edit.rs`:

```rust
use async_trait::async_trait;
use harness_core::{
    message::ToolSpec,
    permission::Action,
    tool::{Tool, ToolContext, ToolOutput},
};
use serde_json::{json, Value};
use similar::TextDiff;

pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit".into(),
            description: "Replace an exact string in a file. old_string must match exactly once unless replace_all is true. Read the file first.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean"}
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
        }
    }

    fn action(&self, args: &Value, ctx: &ToolContext) -> Action {
        Action::Write(ctx.resolve(args["path"].as_str().unwrap_or_default()))
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let path = ctx.resolve(args["path"].as_str().unwrap_or_default());
        let old = args["old_string"].as_str().unwrap_or_default();
        let new = args["new_string"].as_str().unwrap_or_default();
        let replace_all = args["replace_all"].as_bool().unwrap_or(false);
        if old.is_empty() {
            return ToolOutput::error("old_string must not be empty");
        }
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(e) => return ToolOutput::error(format!("cannot read {}: {e}", path.display())),
        };
        if let Err(message) = ctx.tracker.check_fresh(&path, &bytes) {
            return ToolOutput::error(message);
        }
        let Ok(text) = String::from_utf8(bytes) else {
            return ToolOutput::error(format!("{} is not valid UTF-8", path.display()));
        };
        let display = path.strip_prefix(&ctx.workspace).unwrap_or(&path).display().to_string();
        match text.matches(old).count() {
            0 => return ToolOutput::error(format!("old_string not found in {display} (0 matches)")),
            n if n > 1 && !replace_all => {
                return ToolOutput::error(format!(
                    "old_string matches {n} times in {display}; include more surrounding text or set replace_all"
                ));
            }
            _ => {}
        }
        let updated = if replace_all { text.replace(old, new) } else { text.replacen(old, new, 1) };
        if let Err(e) = tokio::fs::write(&path, &updated).await {
            return ToolOutput::error(format!("cannot write {}: {e}", path.display()));
        }
        ctx.tracker.record(&path, updated.as_bytes());
        let diff = TextDiff::from_lines(&text, &updated).unified_diff().header(&display, &display).to_string();
        ToolOutput::ok(diff)
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p harness-tools --test fs_tools && cargo clippy -p harness-tools --all-targets -- -D warnings`
Expected: 12 tests pass; clippy clean.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/harness-tools
git commit -m "feat(tools): add read, write, and edit tools with stale-read protection"
```

---

### Task 7: `grep` and `glob` tools

**Files:**
- Create: `crates/harness-tools/src/walk.rs`, `src/grep.rs`, `src/glob.rs`
- Modify: `crates/harness-tools/src/lib.rs`
- Test: `crates/harness-tools/tests/search_tools.rs`

**Interfaces:**
- Consumes: `Tool`, `ToolContext`, `ToolOutput` (Task 5); `is_binary` (Task 6).
- Produces: `harness_tools::{GrepTool, GlobTool}` (named `grep`, `glob`); `harness_tools::walk::files(root: &Path) -> Vec<PathBuf>` (sorted; honours `.gitignore` even outside git repos; includes hidden files; skips `.git`).

- [ ] **Step 1: Register the modules**

`crates/harness-tools/src/lib.rs` becomes:

```rust
//! The built-in tools: read, write, edit, bash, grep, glob.

pub mod edit;
pub mod glob;
pub mod grep;
pub mod read;
pub mod walk;
pub mod write;

pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use read::ReadTool;
pub use write::WriteTool;
```

Create empty `src/walk.rs`, `src/grep.rs`, `src/glob.rs`.

- [ ] **Step 2: Write the failing tests**

`crates/harness-tools/tests/search_tools.rs`:

```rust
use std::path::Path;

use harness_core::tool::{Tool, ToolContext};
use harness_tools::{GlobTool, GrepTool};
use serde_json::json;

fn put(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn project() -> (tempfile::TempDir, ToolContext) {
    let dir = tempfile::tempdir().unwrap();
    put(dir.path(), ".gitignore", "target/\n");
    put(dir.path(), "src/main.rs", "use std::io;\nfn main() {}\n");
    put(dir.path(), "src/notes.txt", "fn main is mentioned here\n");
    put(dir.path(), "target/debug/gen.rs", "fn main() {}\n");
    put(dir.path(), ".github/ci.yml", "on: push\n");
    let ctx = ToolContext::new(dir.path());
    (dir, ctx)
}

#[tokio::test]
async fn glob_matches_paths_and_respects_gitignore() {
    let (_dir, ctx) = project();
    let out = GlobTool.run(json!({"pattern": "**/*.rs"}), &ctx).await;
    assert!(!out.is_error);
    assert_eq!(out.content.trim(), "src/main.rs");
}

#[tokio::test]
async fn glob_includes_hidden_directories() {
    let (_dir, ctx) = project();
    let out = GlobTool.run(json!({"pattern": "**/*.yml"}), &ctx).await;
    assert_eq!(out.content.trim(), ".github/ci.yml");
}

#[tokio::test]
async fn glob_reports_when_nothing_matches() {
    let (_dir, ctx) = project();
    let out = GlobTool.run(json!({"pattern": "**/*.py"}), &ctx).await;
    assert!(!out.is_error);
    assert!(out.content.contains("No files matched"));
}

#[tokio::test]
async fn grep_returns_path_line_and_text_and_skips_ignored_files() {
    let (_dir, ctx) = project();
    let out = GrepTool.run(json!({"pattern": "fn main\\(\\)"}), &ctx).await;
    assert!(!out.is_error);
    assert_eq!(out.content.trim(), "src/main.rs:2:fn main() {}");
}

#[tokio::test]
async fn grep_filters_files_by_glob() {
    let (_dir, ctx) = project();
    let out = GrepTool.run(json!({"pattern": "fn main", "glob": "*.txt"}), &ctx).await;
    assert_eq!(out.content.trim(), "src/notes.txt:1:fn main is mentioned here");
}

#[tokio::test]
async fn grep_caps_results() {
    let dir = tempfile::tempdir().unwrap();
    put(dir.path(), "big.txt", &"match me\n".repeat(300));
    let ctx = ToolContext::new(dir.path());
    let out = GrepTool.run(json!({"pattern": "match"}), &ctx).await;
    assert!(out.content.contains("100 more matches not shown"), "{}", out.content);
}

#[tokio::test]
async fn grep_rejects_an_invalid_regex() {
    let (_dir, ctx) = project();
    let out = GrepTool.run(json!({"pattern": "("}), &ctx).await;
    assert!(out.is_error);
    assert!(out.content.contains("invalid regex"));
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p harness-tools --test search_tools`
Expected: FAIL to compile: `GlobTool` and `GrepTool` are not defined.

- [ ] **Step 4: Implement walking, glob, and grep**

`crates/harness-tools/src/walk.rs`:

```rust
use std::path::{Path, PathBuf};

/// All files under `root`, sorted. Honours `.gitignore` (even outside a git repository), includes hidden
/// files, and never descends into `.git`.
pub fn files(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
        .map(|entry| entry.into_path())
        .collect();
    out.sort();
    out
}
```

`crates/harness-tools/src/glob.rs`:

```rust
use async_trait::async_trait;
use harness_core::{
    message::ToolSpec,
    permission::Action,
    tool::{Tool, ToolContext, ToolOutput},
};
use serde_json::{json, Value};

use crate::walk;

const MAX_RESULTS: usize = 200;

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob".into(),
            description: "Find files by glob pattern (e.g. src/**/*.rs). Respects .gitignore.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string", "description": "Directory to search; defaults to the workspace"}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }

    fn action(&self, args: &Value, ctx: &ToolContext) -> Action {
        Action::Read(ctx.resolve(args["path"].as_str().unwrap_or(".")))
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let base = ctx.resolve(args["path"].as_str().unwrap_or("."));
        let pattern = args["pattern"].as_str().unwrap_or_default().to_string();
        let matcher = match globset::Glob::new(&pattern) {
            Ok(glob) => glob.compile_matcher(),
            Err(e) => return ToolOutput::error(format!("invalid glob `{pattern}`: {e}")),
        };
        let hits = tokio::task::spawn_blocking(move || {
            walk::files(&base)
                .into_iter()
                .filter_map(|path| {
                    let rel = path.strip_prefix(&base).ok()?.to_path_buf();
                    matcher.is_match(&rel).then(|| rel.display().to_string())
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        if hits.is_empty() {
            return ToolOutput::ok(format!("No files matched `{pattern}`"));
        }
        let mut out = hits.iter().take(MAX_RESULTS).cloned().collect::<Vec<_>>().join("\n");
        if hits.len() > MAX_RESULTS {
            out.push_str(&format!("\n[... {} more files not shown; narrow the pattern]", hits.len() - MAX_RESULTS));
        }
        ToolOutput::ok(out)
    }
}
```

`crates/harness-tools/src/grep.rs`:

```rust
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use harness_core::{
    message::ToolSpec,
    permission::Action,
    tool::{Tool, ToolContext, ToolOutput},
};
use regex::Regex;
use serde_json::{json, Value};

use crate::{read::is_binary, walk};

const MAX_MATCHES: usize = 200;
const MAX_LINE_CHARS: usize = 500;

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".into(),
            description: "Search file contents with a regular expression. Returns path:line:text. Respects .gitignore.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string", "description": "File or directory; defaults to the workspace"},
                    "glob": {"type": "string", "description": "Only search files matching this glob, e.g. *.rs"}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }

    fn action(&self, args: &Value, ctx: &ToolContext) -> Action {
        Action::Read(ctx.resolve(args["path"].as_str().unwrap_or(".")))
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let pattern = args["pattern"].as_str().unwrap_or_default();
        let regex = match Regex::new(pattern) {
            Ok(regex) => regex,
            Err(e) => return ToolOutput::error(format!("invalid regex `{pattern}`: {e}")),
        };
        let filter = match args["glob"].as_str().map(globset::Glob::new) {
            None => None,
            Some(Ok(glob)) => Some(glob.compile_matcher()),
            Some(Err(e)) => return ToolOutput::error(format!("invalid glob: {e}")),
        };
        let base = ctx.resolve(args["path"].as_str().unwrap_or("."));
        let workspace = ctx.workspace.clone();
        tokio::task::spawn_blocking(move || search(&regex, filter.as_ref(), &base, &workspace))
            .await
            .unwrap_or_else(|e| ToolOutput::error(format!("grep failed: {e}")))
    }
}

fn search(regex: &Regex, filter: Option<&globset::GlobMatcher>, base: &Path, workspace: &Path) -> ToolOutput {
    let files: Vec<PathBuf> = if base.is_file() { vec![base.to_path_buf()] } else { walk::files(base) };
    let mut shown = Vec::new();
    let mut total = 0usize;
    for file in files {
        let rel = file.strip_prefix(workspace).unwrap_or(&file).to_path_buf();
        if let Some(filter) = filter {
            let name_matches = file.file_name().is_some_and(|n| filter.is_match(n));
            if !name_matches && !filter.is_match(&rel) {
                continue;
            }
        }
        let Ok(bytes) = std::fs::read(&file) else { continue };
        if is_binary(&bytes) {
            continue;
        }
        for (index, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
            if regex.is_match(line) {
                total += 1;
                if shown.len() < MAX_MATCHES {
                    let text: String = line.chars().take(MAX_LINE_CHARS).collect();
                    shown.push(format!("{}:{}:{}", rel.display(), index + 1, text));
                }
            }
        }
    }
    if total == 0 {
        return ToolOutput::ok("No matches");
    }
    let mut out = shown.join("\n");
    if total > MAX_MATCHES {
        out.push_str(&format!("\n[... {} more matches not shown; narrow the pattern or path]", total - MAX_MATCHES));
    }
    ToolOutput::ok(out)
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p harness-tools --test search_tools && cargo clippy -p harness-tools --all-targets -- -D warnings`
Expected: 7 tests pass; clippy clean.

- [ ] **Step 6: Commit**

```bash
git add crates/harness-tools
git commit -m "feat(tools): add gitignore-aware grep and glob tools"
```

---

### Task 8: `bash` tool and the built-in tool registry

**Files:**
- Create: `crates/harness-tools/src/bash.rs`
- Modify: `crates/harness-tools/src/lib.rs`
- Test: `crates/harness-tools/tests/bash_tool.rs`, `crates/harness-tools/tests/registry.rs`

**Interfaces:**
- Consumes: `Tool`, `ToolContext` (with `cancel`), `ToolOutput` (Task 5).
- Produces: `harness_tools::BashTool` (named `bash`) and `harness_tools::builtin() -> ToolRegistry` returning the six tools in the fixed order `read, write, edit, bash, grep, glob`.

- [ ] **Step 1: Register the module and the registry function**

`crates/harness-tools/src/lib.rs` becomes:

```rust
//! The built-in tools: read, write, edit, bash, grep, glob.

use std::sync::Arc;

use harness_core::tool::ToolRegistry;

pub mod bash;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod read;
pub mod walk;
pub mod write;

pub use bash::BashTool;
pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use read::ReadTool;
pub use write::WriteTool;

/// The six built-in tools in their fixed order.
pub fn builtin() -> ToolRegistry {
    ToolRegistry::new(vec![
        Arc::new(ReadTool),
        Arc::new(WriteTool),
        Arc::new(EditTool),
        Arc::new(BashTool),
        Arc::new(GrepTool),
        Arc::new(GlobTool),
    ])
}
```

Create empty `src/bash.rs`.

- [ ] **Step 2: Write the failing tests**

`crates/harness-tools/tests/bash_tool.rs`:

```rust
use std::time::{Duration, Instant};

use harness_core::tool::{Tool, ToolContext};
use harness_tools::BashTool;
use serde_json::json;

fn ctx() -> (tempfile::TempDir, ToolContext) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext::new(dir.path());
    (dir, ctx)
}

#[tokio::test]
async fn combines_stdout_and_stderr_and_reports_the_exit_code() {
    let (_dir, ctx) = ctx();
    let out = BashTool.run(json!({"command": "echo out; echo err >&2"}), &ctx).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.starts_with("exit code 0\n"));
    assert!(out.content.contains("out") && out.content.contains("err"));
}

#[tokio::test]
async fn runs_in_the_workspace() {
    let (dir, ctx) = ctx();
    std::fs::write(dir.path().join("marker.txt"), "").unwrap();
    let out = BashTool.run(json!({"command": "ls"}), &ctx).await;
    assert!(out.content.contains("marker.txt"));
}

#[tokio::test]
async fn nonzero_exit_is_an_error_result() {
    let (_dir, ctx) = ctx();
    let out = BashTool.run(json!({"command": "echo failing; exit 3"}), &ctx).await;
    assert!(out.is_error);
    assert!(out.content.starts_with("exit code 3\n"));
    assert!(out.content.contains("failing"));
}

#[tokio::test]
async fn timeout_kills_the_whole_process_group() {
    let (dir, ctx) = ctx();
    let started = Instant::now();
    let out = BashTool
        .run(json!({"command": "sleep 30 & echo $! > child.pid; wait", "timeout_secs": 1}), &ctx)
        .await;
    assert!(out.is_error);
    assert!(out.content.contains("timed out after 1s"), "{}", out.content);
    assert!(started.elapsed() < Duration::from_secs(5));

    let pid: i32 = std::fs::read_to_string(dir.path().join("child.pid")).unwrap().trim().parse().unwrap();
    let mut gone = false;
    for _ in 0..20 {
        if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(gone, "background child {pid} survived the timeout");
}

// Review Focus: a background process holding stdout open.
#[tokio::test]
async fn background_process_holding_stdout_does_not_hang_past_the_timeout() {
    let (_dir, ctx) = ctx();
    let started = Instant::now();
    let out = BashTool.run(json!({"command": "sleep 30 & echo started", "timeout_secs": 1}), &ctx).await;
    assert!(out.is_error);
    assert!(out.content.contains("timed out"));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn cancellation_interrupts_a_running_command() {
    let (_dir, ctx) = ctx();
    let cancel = ctx.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel.cancel();
    });
    let started = Instant::now();
    let out = BashTool.run(json!({"command": "sleep 600"}), &ctx).await;
    assert!(out.is_error);
    assert!(out.content.contains("interrupted"));
    assert!(started.elapsed() < Duration::from_secs(2));
}
```

Add `nix.workspace = true` under `[dev-dependencies]` in `crates/harness-tools/Cargo.toml`.

`crates/harness-tools/tests/registry.rs`:

```rust
#[test]
fn builtin_tools_are_ordered_stable_and_compact() {
    let names: Vec<String> = harness_tools::builtin().specs().into_iter().map(|s| s.name).collect();
    assert_eq!(names, ["read", "write", "edit", "bash", "grep", "glob"]);

    let first = serde_json::to_string(&harness_tools::builtin().specs()).unwrap();
    let second = serde_json::to_string(&harness_tools::builtin().specs()).unwrap();
    assert_eq!(first, second, "tool definitions must be byte-identical across requests");
    assert!(first.len() / 4 <= 1500, "tool definitions are ~{} tokens", first.len() / 4);
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p harness-tools --test bash_tool --test registry`
Expected: FAIL to compile: `BashTool` is not defined.

- [ ] **Step 4: Implement the bash tool**

`crates/harness-tools/src/bash.rs`:

```rust
use std::{process::Stdio, time::Duration};

use async_trait::async_trait;
use harness_core::{
    message::ToolSpec,
    permission::Action,
    tool::{Tool, ToolContext, ToolOutput},
};
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 600;

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "Run a non-interactive shell command in the workspace. Returns the exit code and combined stdout/stderr. Default timeout 120s, max 600s.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 600}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    fn action(&self, args: &Value, _ctx: &ToolContext) -> Action {
        Action::Bash(args["command"].as_str().unwrap_or_default().to_string())
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let command = args["command"].as_str().unwrap_or_default();
        let secs = args["timeout_secs"].as_u64().unwrap_or(DEFAULT_TIMEOUT_SECS).clamp(1, MAX_TIMEOUT_SECS);

        // `exec 2>&1` merges stderr into stdout for the whole script, preserving interleaving.
        let mut child = match tokio::process::Command::new("sh")
            .arg("-c")
            .arg(format!("exec 2>&1\n{command}"))
            .current_dir(&ctx.workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(e) => return ToolOutput::error(format!("failed to start sh: {e}")),
        };
        let pgid = child.id().map(|id| id as i32);
        let mut stdout = child.stdout.take().expect("stdout is piped");

        let finished = async {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf).await;
            (buf, child.wait().await)
        };

        tokio::select! {
            (buf, status) = finished => {
                let text = String::from_utf8_lossy(&buf).into_owned();
                match status {
                    Ok(status) => {
                        let code = status.code().map_or_else(|| "signal".to_string(), |c| c.to_string());
                        let body = format!("exit code {code}\n{text}");
                        if status.success() { ToolOutput::ok(body) } else { ToolOutput::error(body) }
                    }
                    Err(e) => ToolOutput::error(format!("failed to wait for command: {e}\n{text}")),
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(secs)) => {
                kill_group(pgid);
                ToolOutput::error(format!("command timed out after {secs}s and was terminated"))
            }
            _ = ctx.cancel.cancelled() => {
                kill_group(pgid);
                ToolOutput::error("command interrupted by the user")
            }
        }
    }
}

/// Kills the command and everything it started (it runs in its own process group).
fn kill_group(pgid: Option<i32>) {
    if let Some(pgid) = pgid {
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), nix::sys::signal::Signal::SIGKILL);
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p harness-tools && cargo clippy -p harness-tools --all-targets -- -D warnings`
Expected: all harness-tools tests pass (Tasks 6–8); clippy clean.

- [ ] **Step 6: Commit**

```bash
git add crates/harness-tools
git commit -m "feat(tools): add bash tool with timeouts, process-group kill, and the builtin registry"
```

---

### Task 9: OpenAI Chat Completions adapter

**Files:**
- Create: `crates/harness-providers/Cargo.toml`, `src/lib.rs`, `src/openai_chat.rs`
- Modify: `Cargo.toml` (add `harness-providers` to `[workspace.dependencies]`)
- Test: `crates/harness-providers/tests/openai_chat_parser.rs`, `crates/harness-providers/tests/openai_chat_http.rs`

**Interfaces:**
- Consumes: `ChatRequest`, `Message`, `ToolCall`, `Usage` (Task 2); `Provider`, `ProviderStream`, `ProviderEvent`, `ProviderError`, `FinishReason` (Task 2).
- Produces:
  - `harness_providers::openai_chat::request_body(&ChatRequest) -> serde_json::Value`
  - `harness_providers::openai_chat::ChatStreamParser` (`Default`) with `push(&mut self, data: &str) -> Result<Vec<ProviderEvent>, ProviderError>`, `finish(&mut self) -> Vec<ProviderEvent>` (idempotent; flushes tool calls then `Finished`), `is_done(&self) -> bool`.
  - `harness_providers::openai_chat::OpenAiChat::new(base_url: impl Into<String>, api_key: Option<String>)` implementing `Provider` (POST `{base_url}/chat/completions`, SSE).

- [ ] **Step 1: Create the crate manifest and register it**

Add to root `Cargo.toml` under `[workspace.dependencies]`:

```toml
harness-providers = { path = "crates/harness-providers" }
```

`crates/harness-providers/Cargo.toml` (declares everything Tasks 9–10 need):

```toml
[package]
name = "harness-providers"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
async-stream.workspace = true
eventsource-stream.workspace = true
futures.workspace = true
harness-config.workspace = true
harness-core.workspace = true
reqwest.workspace = true
serde_json.workspace = true
thiserror.workspace = true
tokio.workspace = true

[dev-dependencies]
tokio.workspace = true
wiremock.workspace = true
```

`crates/harness-providers/src/lib.rs`:

```rust
//! Model provider adapters, local-server discovery, and model-id resolution.

pub mod openai_chat;
```

Create empty `src/openai_chat.rs`.

- [ ] **Step 2: Write the failing parser tests**

`crates/harness-providers/tests/openai_chat_parser.rs`:

```rust
use harness_core::message::{ChatRequest, Message, ToolCall, ToolSpec, Usage};
use harness_core::provider::{FinishReason, ProviderError, ProviderEvent};
use harness_providers::openai_chat::{request_body, ChatStreamParser};
use serde_json::json;

fn parse(chunks: &[&str]) -> Vec<ProviderEvent> {
    let mut parser = ChatStreamParser::default();
    let mut events = Vec::new();
    for chunk in chunks {
        events.extend(parser.push(chunk).unwrap());
    }
    events.extend(parser.finish());
    events
}

#[test]
fn text_usage_and_finish() {
    let events = parse(&[
        r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"Hel"},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":"stop"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":8}}}"#,
        "[DONE]",
    ]);
    assert_eq!(
        events,
        vec![
            ProviderEvent::TextDelta("Hel".into()),
            ProviderEvent::TextDelta("lo".into()),
            ProviderEvent::Usage(Usage { input_tokens: 12, output_tokens: 2, cached_tokens: 8 }),
            ProviderEvent::Finished(FinishReason::Stop),
        ]
    );
}

#[test]
fn tool_call_fragments_are_assembled() {
    let events = parse(&[
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"read","arguments":""}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        "[DONE]",
    ]);
    assert_eq!(
        events,
        vec![
            ProviderEvent::ToolCall(ToolCall { id: "call_a".into(), name: "read".into(), arguments: r#"{"path":"a.txt"}"#.into() }),
            ProviderEvent::Finished(FinishReason::ToolCalls),
        ]
    );
}

// Review Focus: servers that omit tool-call ids or indexes.
#[test]
fn missing_ids_and_indexes_still_yield_distinct_calls() {
    let events = parse(&[
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"function":{"name":"read","arguments":{"path":"a"}}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"function":{"name":"glob","arguments":"{\"pattern\":\"*\"}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
    ]);
    assert_eq!(
        events,
        vec![
            ProviderEvent::ToolCall(ToolCall { id: "call_0".into(), name: "read".into(), arguments: r#"{"path":"a"}"#.into() }),
            ProviderEvent::ToolCall(ToolCall { id: "call_1".into(), name: "glob".into(), arguments: r#"{"pattern":"*"}"#.into() }),
            ProviderEvent::Finished(FinishReason::ToolCalls),
        ]
    );
}

#[test]
fn empty_arguments_become_an_empty_object() {
    let events = parse(&[r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"glob"}}]},"finish_reason":"tool_calls"}]}"#]);
    assert_eq!(events[0], ProviderEvent::ToolCall(ToolCall { id: "c".into(), name: "glob".into(), arguments: "{}".into() }));
}

#[test]
fn reasoning_fields_become_reasoning_deltas() {
    let events = parse(&[
        r#"{"choices":[{"index":0,"delta":{"reasoning_content":"think"}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"reasoning":"more"},"finish_reason":"length"}]}"#,
    ]);
    assert_eq!(
        events,
        vec![
            ProviderEvent::ReasoningDelta("think".into()),
            ProviderEvent::ReasoningDelta("more".into()),
            ProviderEvent::Finished(FinishReason::Length),
        ]
    );
}

#[test]
fn invalid_json_and_error_payloads_are_protocol_errors() {
    let mut parser = ChatStreamParser::default();
    assert!(matches!(parser.push("{not json"), Err(ProviderError::Protocol(_))));
    assert!(matches!(parser.push(r#"{"error":{"message":"model not found"}}"#), Err(ProviderError::Protocol(_))));
}

#[test]
fn finish_is_idempotent() {
    let mut parser = ChatStreamParser::default();
    parser.push("[DONE]").unwrap();
    assert!(parser.is_done());
    assert!(parser.finish().is_empty());
}

#[test]
fn request_body_maps_messages_and_tools() {
    let req = ChatRequest {
        model: "qwen3:14b".into(),
        system: "be brief".into(),
        messages: vec![
            Message::User { content: "hi".into() },
            Message::Assistant {
                content: String::new(),
                tool_calls: vec![ToolCall { id: "c1".into(), name: "read".into(), arguments: "{}".into() }],
                model: "ollama/qwen3:14b".into(),
            },
            Message::Tool { call_id: "c1".into(), content: "data".into(), is_error: false },
        ],
        tools: vec![ToolSpec { name: "read".into(), description: "Read".into(), parameters: json!({"type": "object"}) }],
    };
    let body = request_body(&req);
    assert_eq!(body["model"], "qwen3:14b");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(body["messages"][0], json!({"role": "system", "content": "be brief"}));
    assert_eq!(body["messages"][1], json!({"role": "user", "content": "hi"}));
    assert_eq!(body["messages"][2]["content"], serde_json::Value::Null);
    assert_eq!(body["messages"][2]["tool_calls"][0]["function"]["name"], "read");
    assert_eq!(body["messages"][3], json!({"role": "tool", "tool_call_id": "c1", "content": "data"}));
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["function"]["name"], "read");
}

#[test]
fn request_body_omits_empty_tools() {
    let req = ChatRequest { model: "m".into(), system: String::new(), messages: vec![], tools: vec![] };
    assert!(request_body(&req).get("tools").is_none());
}
```

- [ ] **Step 3: Write the failing HTTP tests**

`crates/harness-providers/tests/openai_chat_http.rs`:

```rust
use std::time::Duration;

use futures::StreamExt;
use harness_core::message::ChatRequest;
use harness_core::provider::{FinishReason, Provider, ProviderError, ProviderEvent};
use harness_providers::openai_chat::OpenAiChat;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse(chunks: &[&str]) -> String {
    chunks.iter().map(|c| format!("data: {c}\n\n")).collect()
}

fn request() -> ChatRequest {
    ChatRequest { model: "m".into(), system: "s".into(), messages: vec![], tools: vec![] }
}

#[tokio::test]
async fn streams_events_from_the_server_with_the_api_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-test"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&[r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#, "[DONE]"]),
            "text/event-stream",
        ))
        .mount(&server)
        .await;

    let provider = OpenAiChat::new(format!("{}/v1", server.uri()), Some("sk-test".into()));
    let events: Vec<_> = provider.stream(request()).collect().await;
    assert_eq!(
        events,
        vec![Ok(ProviderEvent::TextDelta("hi".into())), Ok(ProviderEvent::Finished(FinishReason::Stop))]
    );
}

#[tokio::test]
async fn http_errors_carry_status_body_and_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "3").set_body_string("slow down"))
        .mount(&server)
        .await;
    let provider = OpenAiChat::new(format!("{}/v1", server.uri()), None);
    let first = provider.stream(request()).next().await.unwrap();
    assert_eq!(
        first,
        Err(ProviderError::Http { status: 429, body: "slow down".into(), retry_after: Some(Duration::from_secs(3)) })
    );
}

#[tokio::test]
async fn http_date_retry_after_is_ignored_safely() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT"))
        .mount(&server)
        .await;
    let provider = OpenAiChat::new(format!("{}/v1", server.uri()), None);
    let first = provider.stream(request()).next().await.unwrap();
    assert!(matches!(first, Err(ProviderError::Http { status: 503, retry_after: None, .. })));
}

#[tokio::test]
async fn unreachable_servers_are_network_errors() {
    let provider = OpenAiChat::new("http://127.0.0.1:9/v1", None);
    let first = provider.stream(request()).next().await.unwrap();
    assert!(matches!(first, Err(ProviderError::Network(_))));
}
```

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo test -p harness-providers`
Expected: FAIL to compile: `request_body`, `ChatStreamParser`, and `OpenAiChat` are not defined.

- [ ] **Step 5: Implement the adapter**

`crates/harness-providers/src/openai_chat.rs`:

```rust
use std::{collections::BTreeMap, time::Duration};

use eventsource_stream::Eventsource;
use futures::StreamExt;
use harness_core::{
    message::{ChatRequest, Message, ToolCall, Usage},
    provider::{FinishReason, Provider, ProviderError, ProviderEvent, ProviderStream},
};
use serde_json::{json, Value};

/// Builds a streaming Chat Completions request body.
pub fn request_body(req: &ChatRequest) -> Value {
    let mut messages = vec![json!({"role": "system", "content": req.system})];
    for message in &req.messages {
        messages.push(match message {
            Message::User { content } => json!({"role": "user", "content": content}),
            Message::Assistant { content, tool_calls, .. } => {
                let text = if content.is_empty() && !tool_calls.is_empty() { Value::Null } else { json!(content) };
                let mut value = json!({"role": "assistant", "content": text});
                if !tool_calls.is_empty() {
                    value["tool_calls"] = Value::Array(
                        tool_calls
                            .iter()
                            .map(|c| json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": c.arguments}}))
                            .collect(),
                    );
                }
                value
            }
            Message::Tool { call_id, content, .. } => json!({"role": "tool", "tool_call_id": call_id, "content": content}),
        });
    }
    let mut body = json!({
        "model": req.model,
        "stream": true,
        "stream_options": {"include_usage": true},
        "messages": messages,
    });
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.parameters}}))
                .collect(),
        );
    }
    body
}

#[derive(Debug, Default)]
struct PartialCall {
    id: Option<String>,
    name: String,
    arguments: String,
}

/// Turns SSE `data:` payloads into [`ProviderEvent`]s. Tool calls are buffered and emitted whole at the end.
#[derive(Debug, Default)]
pub struct ChatStreamParser {
    calls: BTreeMap<u64, PartialCall>,
    finish: Option<FinishReason>,
    done: bool,
}

impl ChatStreamParser {
    pub fn push(&mut self, data: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
        if data.trim() == "[DONE]" {
            return Ok(self.finish());
        }
        let chunk: Value =
            serde_json::from_str(data).map_err(|e| ProviderError::Protocol(format!("{e} in chunk: {data}")))?;
        if let Some(error) = chunk.get("error") {
            return Err(ProviderError::Protocol(format!("provider error: {error}")));
        }
        let mut out = Vec::new();
        if let Some(choice) = chunk["choices"].get(0) {
            let delta = &choice["delta"];
            if let Some(text) = delta["content"].as_str().filter(|t| !t.is_empty()) {
                out.push(ProviderEvent::TextDelta(text.to_string()));
            }
            for key in ["reasoning_content", "reasoning"] {
                if let Some(text) = delta[key].as_str().filter(|t| !t.is_empty()) {
                    out.push(ProviderEvent::ReasoningDelta(text.to_string()));
                }
            }
            if let Some(calls) = delta["tool_calls"].as_array() {
                for call in calls {
                    let starts_new = call["id"].is_string() || call["function"]["name"].is_string();
                    let index = match call["index"].as_u64() {
                        Some(index) => index,
                        None if starts_new => self.calls.len() as u64,
                        None => self.calls.keys().last().copied().unwrap_or(0),
                    };
                    let entry = self.calls.entry(index).or_default();
                    if let Some(id) = call["id"].as_str() {
                        entry.id = Some(id.to_string());
                    }
                    if let Some(name) = call["function"]["name"].as_str() {
                        entry.name.push_str(name);
                    }
                    match &call["function"]["arguments"] {
                        Value::String(fragment) => entry.arguments.push_str(fragment),
                        Value::Null => {}
                        other => entry.arguments.push_str(&other.to_string()),
                    }
                }
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                self.finish = Some(match reason {
                    "stop" => FinishReason::Stop,
                    "tool_calls" => FinishReason::ToolCalls,
                    "length" => FinishReason::Length,
                    other => FinishReason::Other(other.to_string()),
                });
            }
        }
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            out.push(ProviderEvent::Usage(Usage {
                input_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0),
                output_tokens: usage["completion_tokens"].as_u64().unwrap_or(0),
                cached_tokens: usage["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0),
            }));
        }
        Ok(out)
    }

    /// Emits buffered tool calls followed by `Finished`. Calling it again yields nothing.
    pub fn finish(&mut self) -> Vec<ProviderEvent> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        let mut out: Vec<ProviderEvent> = std::mem::take(&mut self.calls)
            .into_iter()
            .map(|(index, call)| {
                ProviderEvent::ToolCall(ToolCall {
                    id: call.id.unwrap_or_else(|| format!("call_{index}")),
                    name: call.name,
                    arguments: if call.arguments.trim().is_empty() { "{}".into() } else { call.arguments },
                })
            })
            .collect();
        out.push(ProviderEvent::Finished(self.finish.take().unwrap_or(FinishReason::Stop)));
        out
    }

    pub fn is_done(&self) -> bool {
        self.done
    }
}

/// A provider speaking the OpenAI Chat Completions protocol (Ollama, LM Studio, llama.cpp, OpenRouter, ...).
pub struct OpenAiChat {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl OpenAiChat {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        OpenAiChat { client: reqwest::Client::new(), base_url: base_url.into().trim_end_matches('/').to_string(), api_key }
    }
}

impl Provider for OpenAiChat {
    fn stream(&self, request: ChatRequest) -> ProviderStream {
        let mut http = self.client.post(format!("{}/chat/completions", self.base_url)).json(&request_body(&request));
        if let Some(key) = &self.api_key {
            http = http.bearer_auth(key);
        }
        Box::pin(async_stream::try_stream! {
            let response = http.send().await.map_err(|e| ProviderError::Network(e.to_string()))?;
            let status = response.status();
            if !status.is_success() {
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .map(Duration::from_secs);
                let body = response.text().await.unwrap_or_default();
                // `?` on an `Err` ends the stream with this error.
                Err::<(), ProviderError>(ProviderError::Http { status: status.as_u16(), body, retry_after })?;
            }
            let mut parser = ChatStreamParser::default();
            let mut events = response.bytes_stream().eventsource();
            while let Some(event) = events.next().await {
                let event = event.map_err(|e| ProviderError::Network(e.to_string()))?;
                for item in parser.push(&event.data)? {
                    yield item;
                }
                if parser.is_done() {
                    break;
                }
            }
            for item in parser.finish() {
                yield item;
            }
        })
    }
}
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p harness-providers && cargo clippy -p harness-providers --all-targets -- -D warnings`
Expected: 13 tests pass; clippy clean. If the compiler cannot infer the stream's error type inside `try_stream!`, annotate the stream as `let stream: ProviderStream = Box::pin(async_stream::try_stream! { ... }); stream` rather than changing behaviour.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock crates/harness-providers
git commit -m "feat(providers): add streaming OpenAI Chat Completions adapter"
```

---

### Task 10: Local discovery and model-id resolution

**Files:**
- Create: `crates/harness-providers/src/discovery.rs`, `crates/harness-providers/src/registry.rs`
- Modify: `crates/harness-providers/src/lib.rs`
- Test: `crates/harness-providers/tests/discovery.rs`, `crates/harness-providers/tests/registry.rs`

**Interfaces:**
- Consumes: `OpenAiChat` (Task 9); `ProviderConfig`, `Protocol` (Task 3); `Provider` (Task 2).
- Produces:
  - `harness_providers::discovery::{DiscoveredModel, Endpoint, list_models, LOCAL_PROBE_TIMEOUT, REMOTE_PROBE_TIMEOUT}`; `DiscoveredModel { provider, name }` with `id() -> String`; `Endpoint { provider, base_url, api_key: Option<String> }`; `async fn list_models(endpoints: &[Endpoint], timeout: Duration) -> Vec<DiscoveredModel>`.
  - `harness_providers::registry::{BUILTIN_PROVIDERS, Resolved, ResolveError, resolve, local_endpoints, configured_endpoints}`; `Resolved { provider: Arc<dyn Provider>, model: String, id: String }`; `resolve(model_id: &str, providers: &BTreeMap<String, ProviderConfig>, env: impl Fn(&str) -> Option<String>) -> Result<Resolved, ResolveError>`; `local_endpoints(providers) -> Vec<Endpoint>` (the three local servers, minus any name the user configured); `configured_endpoints(providers, env) -> Vec<Endpoint>` (configured providers whose key, if required, is present).

- [ ] **Step 1: Register the modules**

`crates/harness-providers/src/lib.rs` becomes:

```rust
//! Model provider adapters, local-server discovery, and model-id resolution.

pub mod discovery;
pub mod openai_chat;
pub mod registry;
```

Create empty `src/discovery.rs` and `src/registry.rs`.

- [ ] **Step 2: Write the failing tests**

`crates/harness-providers/tests/discovery.rs`:

```rust
use std::time::{Duration, Instant};

use harness_providers::discovery::{list_models, DiscoveredModel, Endpoint};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn endpoint(provider: &str, base_url: String) -> Endpoint {
    Endpoint { provider: provider.into(), base_url, api_key: None }
}

#[tokio::test]
async fn lists_models_sorted_and_skips_unreachable_or_slow_servers() {
    let fast = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "zeta"}, {"id": "alpha"}]})))
        .mount(&fast)
        .await;
    let slow = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "late"}]})).set_delay(Duration::from_secs(2)))
        .mount(&slow)
        .await;

    let started = Instant::now();
    let found = list_models(
        &[
            endpoint("ollama", format!("{}/v1", fast.uri())),
            endpoint("lmstudio", "http://127.0.0.1:9/v1".into()),
            endpoint("llamacpp", format!("{}/v1", slow.uri())),
        ],
        Duration::from_millis(300),
    )
    .await;

    assert!(started.elapsed() < Duration::from_secs(1), "probes must run concurrently and time out");
    assert_eq!(
        found,
        vec![
            DiscoveredModel { provider: "ollama".into(), name: "alpha".into() },
            DiscoveredModel { provider: "ollama".into(), name: "zeta".into() },
        ]
    );
    assert_eq!(found[0].id(), "ollama/alpha");
}

#[tokio::test]
async fn error_statuses_yield_no_models() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(401)).mount(&server).await;
    let found = list_models(&[endpoint("x", format!("{}/v1", server.uri()))], Duration::from_millis(300)).await;
    assert!(found.is_empty());
}
```

`crates/harness-providers/tests/registry.rs`:

```rust
use std::collections::{BTreeMap, HashMap};

use harness_config::config::{Protocol, ProviderConfig};
use harness_providers::registry::{configured_endpoints, local_endpoints, resolve, ResolveError};

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    move |k| map.get(k).cloned()
}

fn custom(name: &str, url: &str, key_env: Option<&str>) -> BTreeMap<String, ProviderConfig> {
    BTreeMap::from([(
        name.to_string(),
        ProviderConfig { protocol: Protocol::OpenaiChat, base_url: url.into(), api_key_env: key_env.map(String::from) },
    )])
}

#[test]
fn resolves_builtin_local_providers() {
    let r = resolve("ollama/qwen3:14b", &BTreeMap::new(), env(&[])).unwrap();
    assert_eq!(r.model, "qwen3:14b");
    assert_eq!(r.id, "ollama/qwen3:14b");
}

#[test]
fn splits_only_at_the_first_slash() {
    let r = resolve("openrouter/qwen/qwen3-coder", &BTreeMap::new(), env(&[("OPENROUTER_API_KEY", "k")])).unwrap();
    assert_eq!(r.model, "qwen/qwen3-coder");
}

#[test]
fn reports_bad_ids_unknown_providers_and_missing_keys() {
    let none = BTreeMap::new();
    assert_eq!(resolve("qwen", &none, env(&[])).err(), Some(ResolveError::BadId("qwen".into())));
    assert_eq!(resolve("ollama/", &none, env(&[])).err(), Some(ResolveError::BadId("ollama/".into())));
    assert_eq!(resolve("nope/m", &none, env(&[])).err(), Some(ResolveError::UnknownProvider("nope".into())));
    assert_eq!(
        resolve("openrouter/m", &none, env(&[])).err(),
        Some(ResolveError::MissingKey { provider: "openrouter".into(), var: "OPENROUTER_API_KEY".into() })
    );
}

#[test]
fn configured_providers_resolve_and_override_builtins() {
    let providers = custom("ollama", "http://gpu-box:11434/v1", None);
    assert_eq!(resolve("ollama/m", &providers, env(&[])).unwrap().model, "m");
    assert!(local_endpoints(&providers).iter().all(|e| e.provider != "ollama"));
}

#[test]
fn configured_endpoints_require_their_key() {
    let providers = custom("work", "https://llm.example/v1", Some("WORK_KEY"));
    assert!(configured_endpoints(&providers, env(&[])).is_empty());
    let with_key = configured_endpoints(&providers, env(&[("WORK_KEY", "k")]));
    assert_eq!(with_key.len(), 1);
    assert_eq!(with_key[0].api_key.as_deref(), Some("k"));
}

#[test]
fn local_endpoints_cover_the_three_servers() {
    let names: Vec<String> = local_endpoints(&BTreeMap::new()).into_iter().map(|e| e.provider).collect();
    assert_eq!(names, ["ollama", "lmstudio", "llamacpp"]);
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p harness-providers --test discovery --test registry`
Expected: FAIL to compile: `discovery` and `registry` items are not defined.

- [ ] **Step 4: Implement discovery and resolution**

`crates/harness-providers/src/discovery.rs`:

```rust
use std::time::Duration;

use serde_json::Value;

/// Each local-server probe gives up after this long, so absent servers never delay startup.
pub const LOCAL_PROBE_TIMEOUT: Duration = Duration::from_millis(300);
/// Remote `/models` listings get longer.
pub const REMOTE_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredModel {
    pub provider: String,
    pub name: String,
}

impl DiscoveredModel {
    pub fn id(&self) -> String {
        format!("{}/{}", self.provider, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub provider: String,
    pub base_url: String,
    pub api_key: Option<String>,
}

/// Lists models from OpenAI-compatible `/models` endpoints concurrently. Unreachable, slow, or failing
/// endpoints are skipped. Results keep endpoint order; models within an endpoint are sorted by name.
pub async fn list_models(endpoints: &[Endpoint], timeout: Duration) -> Vec<DiscoveredModel> {
    let Ok(client) = reqwest::Client::builder().timeout(timeout).build() else {
        return Vec::new();
    };
    let probes = endpoints.iter().map(|endpoint| {
        let client = client.clone();
        async move {
            let mut request = client.get(format!("{}/models", endpoint.base_url.trim_end_matches('/')));
            if let Some(key) = &endpoint.api_key {
                request = request.bearer_auth(key);
            }
            let Ok(response) = request.send().await.and_then(|r| r.error_for_status()) else {
                return Vec::new();
            };
            let Ok(body) = response.json::<Value>().await else {
                return Vec::new();
            };
            let mut names: Vec<String> = body["data"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| m["id"].as_str().map(String::from))
                .collect();
            names.sort();
            names
                .into_iter()
                .map(|name| DiscoveredModel { provider: endpoint.provider.clone(), name })
                .collect::<Vec<_>>()
        }
    });
    futures::future::join_all(probes).await.into_iter().flatten().collect()
}
```

`crates/harness-providers/src/registry.rs`:

```rust
use std::{collections::BTreeMap, sync::Arc};

use harness_config::config::{Protocol, ProviderConfig};
use harness_core::provider::Provider;

use crate::{discovery::Endpoint, openai_chat::OpenAiChat};

/// Providers usable without configuration: (name, base URL, API-key environment variable).
pub const BUILTIN_PROVIDERS: [(&str, &str, Option<&str>); 4] = [
    ("ollama", "http://127.0.0.1:11434/v1", None),
    ("lmstudio", "http://127.0.0.1:1234/v1", None),
    ("llamacpp", "http://127.0.0.1:8080/v1", None),
    ("openrouter", "https://openrouter.ai/api/v1", Some("OPENROUTER_API_KEY")),
];

const LOCAL_PROVIDERS: [&str; 3] = ["ollama", "lmstudio", "llamacpp"];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error("model id `{0}` must look like <provider>/<model>")]
    BadId(String),
    #[error("unknown provider `{0}`; define it under [providers.{0}] in config.toml")]
    UnknownProvider(String),
    #[error("provider `{provider}` needs an API key in ${var}")]
    MissingKey { provider: String, var: String },
}

/// A ready-to-use provider for one model id.
pub struct Resolved {
    pub provider: Arc<dyn Provider>,
    /// The model name sent to the provider (the id without its `<provider>/` prefix).
    pub model: String,
    /// The full `<provider>/<model>` id.
    pub id: String,
}

pub fn resolve(
    model_id: &str,
    providers: &BTreeMap<String, ProviderConfig>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Resolved, ResolveError> {
    let (name, model) = model_id
        .split_once('/')
        .filter(|(p, m)| !p.is_empty() && !m.is_empty())
        .ok_or_else(|| ResolveError::BadId(model_id.to_string()))?;
    let (protocol, base_url, key_env) = if let Some(cfg) = providers.get(name) {
        (cfg.protocol, cfg.base_url.clone(), cfg.api_key_env.clone())
    } else if let Some((_, url, key)) = BUILTIN_PROVIDERS.iter().find(|(n, ..)| *n == name) {
        (Protocol::OpenaiChat, url.to_string(), key.map(String::from))
    } else {
        return Err(ResolveError::UnknownProvider(name.to_string()));
    };
    let api_key = match key_env {
        Some(var) => Some(
            env(&var)
                .filter(|v| !v.is_empty())
                .ok_or(ResolveError::MissingKey { provider: name.to_string(), var })?,
        ),
        None => None,
    };
    let provider: Arc<dyn Provider> = match protocol {
        Protocol::OpenaiChat => Arc::new(OpenAiChat::new(base_url, api_key)),
    };
    Ok(Resolved { provider, model: model.to_string(), id: model_id.to_string() })
}

/// The three local servers, except any the user has redefined in config.
pub fn local_endpoints(providers: &BTreeMap<String, ProviderConfig>) -> Vec<Endpoint> {
    BUILTIN_PROVIDERS
        .iter()
        .filter(|(name, ..)| LOCAL_PROVIDERS.contains(name) && !providers.contains_key(*name))
        .map(|(name, url, _)| Endpoint { provider: name.to_string(), base_url: url.to_string(), api_key: None })
        .collect()
}

/// Configured providers whose API key (if one is required) is present.
pub fn configured_endpoints(
    providers: &BTreeMap<String, ProviderConfig>,
    env: impl Fn(&str) -> Option<String>,
) -> Vec<Endpoint> {
    providers
        .iter()
        .filter_map(|(name, cfg)| {
            let api_key = match &cfg.api_key_env {
                Some(var) => Some(env(var).filter(|v| !v.is_empty())?),
                None => None,
            };
            Some(Endpoint { provider: name.clone(), base_url: cfg.base_url.clone(), api_key })
        })
        .collect()
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p harness-providers && cargo clippy -p harness-providers --all-targets -- -D warnings`
Expected: all harness-providers tests pass; clippy clean.

- [ ] **Step 6: Commit**

```bash
git add crates/harness-providers
git commit -m "feat(providers): add local model discovery and <provider>/<model> resolution"
```

---

### Task 11: The agent loop

**Files:**
- Create: `crates/harness-core/src/agent.rs`, `crates/harness-core/src/testing.rs`
- Modify: `crates/harness-core/src/lib.rs`
- Test: `crates/harness-core/tests/common/mod.rs`, `crates/harness-core/tests/agent.rs`

**Interfaces:**
- Consumes: everything in `harness-core` from Tasks 2, 4, 5.
- Produces:
  - `harness_core::agent::AgentConfig { model_id, model_name, system_prompt, max_steps (default 50), output_limit (default 10 KB), output_dir }` with `AgentConfig::new(model_id, model_name, system_prompt, output_dir: PathBuf)`. Task 12 adds a `retry` field to this struct.
  - `harness_core::agent::{Approver, ApprovalRequest, ApprovalDecision, NonInteractive}`; `ApprovalDecision::{Approve, Deny { feedback: Option<String> }, Unavailable}`.
  - `harness_core::agent::Agent::new(provider: Arc<dyn Provider>, tools: ToolRegistry, policy: Arc<dyn PermissionPolicy>, approver: Arc<dyn Approver>, config: AgentConfig, ctx: ToolContext) -> Agent`
  - `Agent::run_turn(&mut self, input: String, events: &UnboundedSender<AgentEvent>, cancel: CancellationToken) -> TurnEndReason`, `Agent::history() -> &[Message]`, `Agent::config_mut() -> &mut AgentConfig`, `Agent::invalid_calls_this_turn() -> u32`.
  - `harness_core::testing::{MockProvider, Script}`; `Script::{Reply(Vec<Result<ProviderEvent, ProviderError>>), Hang(Vec<ProviderEvent>)}` with helpers `Script::text(&str)`, `Script::tool_call(id, name, Value)`, `Script::raw_tool_call(id, name, &str)`, `Script::error(ProviderError)`; `MockProvider::new(Vec<Script>) -> Arc<MockProvider>`; `MockProvider::requests() -> Vec<ChatRequest>`.

- [ ] **Step 1: Register the modules**

`crates/harness-core/src/lib.rs` becomes:

```rust
//! Core agent runtime for harness: provider-neutral messages, events, permissions, and the agent loop.

pub mod agent;
pub mod event;
pub mod message;
pub mod output;
pub mod permission;
pub mod provider;
pub mod testing;
pub mod tool;
```

`crates/harness-core/src/testing.rs`:

```rust
//! Test doubles. Small and dependency-free, so they are always compiled and usable from any test crate.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use futures::StreamExt;

use crate::{
    message::{ChatRequest, ToolCall},
    provider::{FinishReason, Provider, ProviderError, ProviderEvent, ProviderStream},
};

/// One scripted model response.
pub enum Script {
    /// Yield these items, then end the stream.
    Reply(Vec<Result<ProviderEvent, ProviderError>>),
    /// Yield these events, then never finish (for interrupt tests).
    Hang(Vec<ProviderEvent>),
}

impl Script {
    pub fn text(text: &str) -> Script {
        Script::Reply(vec![
            Ok(ProviderEvent::TextDelta(text.to_string())),
            Ok(ProviderEvent::Finished(FinishReason::Stop)),
        ])
    }

    pub fn tool_call(id: &str, name: &str, args: serde_json::Value) -> Script {
        Self::raw_tool_call(id, name, &args.to_string())
    }

    pub fn raw_tool_call(id: &str, name: &str, arguments: &str) -> Script {
        Script::Reply(vec![
            Ok(ProviderEvent::ToolCall(ToolCall { id: id.into(), name: name.into(), arguments: arguments.into() })),
            Ok(ProviderEvent::Finished(FinishReason::ToolCalls)),
        ])
    }

    pub fn error(error: ProviderError) -> Script {
        Script::Reply(vec![Err(error)])
    }
}

/// A provider that replays a script, one entry per request, and records every request.
pub struct MockProvider {
    script: Mutex<VecDeque<Script>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl MockProvider {
    pub fn new(script: Vec<Script>) -> Arc<MockProvider> {
        Arc::new(MockProvider { script: Mutex::new(script.into()), requests: Mutex::new(Vec::new()) })
    }

    pub fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().expect("requests lock").clone()
    }
}

impl Provider for MockProvider {
    fn stream(&self, request: ChatRequest) -> ProviderStream {
        self.requests.lock().expect("requests lock").push(request);
        match self.script.lock().expect("script lock").pop_front() {
            Some(Script::Reply(items)) => Box::pin(futures::stream::iter(items)),
            Some(Script::Hang(before)) => {
                Box::pin(futures::stream::iter(before.into_iter().map(Ok)).chain(futures::stream::pending()))
            }
            None => Box::pin(futures::stream::iter(vec![Err(ProviderError::Protocol("mock script exhausted".into()))])),
        }
    }
}
```

Create empty `src/agent.rs`.

- [ ] **Step 2: Write the shared test helpers**

`crates/harness-core/tests/common/mod.rs`:

```rust
#![allow(dead_code)]

use std::{path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use harness_core::agent::{Agent, AgentConfig, ApprovalDecision, ApprovalRequest, Approver};
use harness_core::event::{AgentEvent, TurnEndReason};
use harness_core::message::ToolSpec;
use harness_core::permission::{Action, BaselinePolicy, Mode};
use harness_core::testing::MockProvider;
use harness_core::tool::{Tool, ToolContext, ToolOutput, ToolRegistry};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn spec(name: &str, params: Value) -> ToolSpec {
    ToolSpec { name: name.into(), description: format!("test tool {name}"), parameters: params }
}

/// Returns its `text` argument. Reads the workspace, so it is always allowed.
pub struct Echo;
#[async_trait]
impl Tool for Echo {
    fn spec(&self) -> ToolSpec {
        spec("echo", json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"], "additionalProperties": false}))
    }
    fn action(&self, _args: &Value, ctx: &ToolContext) -> Action {
        Action::Read(ctx.workspace.clone())
    }
    async fn run(&self, args: Value, _ctx: &ToolContext) -> ToolOutput {
        ToolOutput::ok(args["text"].as_str().unwrap_or_default())
    }
}

/// Creates an empty file at `path`.
pub struct Touch;
#[async_trait]
impl Tool for Touch {
    fn spec(&self) -> ToolSpec {
        spec("touch", json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}))
    }
    fn action(&self, args: &Value, ctx: &ToolContext) -> Action {
        Action::Write(ctx.resolve(args["path"].as_str().unwrap_or_default()))
    }
    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        std::fs::write(ctx.resolve(args["path"].as_str().unwrap_or_default()), "").unwrap();
        ToolOutput::ok("touched")
    }
}

/// Always fails.
pub struct Fail;
#[async_trait]
impl Tool for Fail {
    fn spec(&self) -> ToolSpec {
        spec("fail", json!({"type": "object"}))
    }
    fn action(&self, _args: &Value, ctx: &ToolContext) -> Action {
        Action::Read(ctx.workspace.clone())
    }
    async fn run(&self, _args: Value, _ctx: &ToolContext) -> ToolOutput {
        ToolOutput::error("tool exploded")
    }
}

/// Waits until the turn is cancelled (or 600 s pass).
pub struct Sleepy;
#[async_trait]
impl Tool for Sleepy {
    fn spec(&self) -> ToolSpec {
        spec("sleepy", json!({"type": "object"}))
    }
    fn action(&self, _args: &Value, ctx: &ToolContext) -> Action {
        Action::Read(ctx.workspace.clone())
    }
    async fn run(&self, _args: Value, ctx: &ToolContext) -> ToolOutput {
        tokio::select! {
            _ = ctx.cancel.cancelled() => ToolOutput::error("interrupted"),
            _ = tokio::time::sleep(Duration::from_secs(600)) => ToolOutput::ok("slept"),
        }
    }
}

pub struct AlwaysApprove;
#[async_trait]
impl Approver for AlwaysApprove {
    async fn decide(&self, _request: &ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Approve
    }
}

pub fn agent(provider: Arc<MockProvider>, mode: Mode, approver: Arc<dyn Approver>, dir: &Path) -> Agent {
    let tools = ToolRegistry::new(vec![Arc::new(Echo), Arc::new(Touch), Arc::new(Fail), Arc::new(Sleepy)]);
    let policy = Arc::new(BaselinePolicy::new(mode, dir, vec![]));
    let config = AgentConfig::new("mock/m1", "m1", "system prompt", dir.join(".spill"));
    Agent::new(provider, tools, policy, approver, config, ToolContext::new(dir))
}

pub async fn run(agent: &mut Agent, input: &str) -> (TurnEndReason, Vec<AgentEvent>) {
    run_with(agent, input, CancellationToken::new()).await
}

pub async fn run_with(agent: &mut Agent, input: &str, cancel: CancellationToken) -> (TurnEndReason, Vec<AgentEvent>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let reason = agent.run_turn(input.to_string(), &tx, cancel).await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    (reason, events)
}

pub fn finished_outputs(events: &[AgentEvent]) -> Vec<(String, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallFinished { output, is_error, .. } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .collect()
}
```

- [ ] **Step 3: Write the failing agent tests**

`crates/harness-core/tests/agent.rs`:

```rust
mod common;

use std::sync::Arc;

use common::*;
use harness_core::agent::NonInteractive;
use harness_core::event::{AgentEvent, TurnEndReason};
use harness_core::message::Message;
use harness_core::permission::Mode;
use harness_core::provider::ProviderError;
use harness_core::testing::{MockProvider, Script};
use serde_json::json;

#[tokio::test]
async fn multi_step_turn_runs_tools_in_order_then_completes() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "echo", json!({"text": "one"})),
        Script::tool_call("c2", "echo", json!({"text": "two"})),
        Script::text("done"),
    ]);
    let mut agent = agent(provider.clone(), Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (reason, events) = run(&mut agent, "go").await;

    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(finished_outputs(&events), vec![("one".to_string(), false), ("two".to_string(), false)]);
    assert_eq!(events.first(), Some(&AgentEvent::TurnStarted));
    assert_eq!(events.last(), Some(&AgentEvent::TurnFinished { reason: TurnEndReason::Completed }));
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[2].messages.iter().any(|m| matches!(m, Message::Tool { content, .. } if content == "two")));
    assert_eq!(requests[0].system, "system prompt");
    assert_eq!(requests[0].model, "m1");
}

#[tokio::test]
async fn tool_call_requested_precedes_finished_with_the_same_id() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::tool_call("c9", "echo", json!({"text": "x"})), Script::text("ok")]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (_, events) = run(&mut agent, "go").await;
    let requested = events.iter().position(|e| matches!(e, AgentEvent::ToolCallRequested { id, .. } if id == "c9")).unwrap();
    let finished = events.iter().position(|e| matches!(e, AgentEvent::ToolCallFinished { id, .. } if id == "c9")).unwrap();
    assert!(requested < finished);
}

#[tokio::test]
async fn assistant_messages_are_attributed_to_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::text("hello")]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (_, events) = run(&mut agent, "hi").await;
    assert!(events.contains(&AgentEvent::AssistantMessage { content: "hello".into(), model: "mock/m1".into() }));
    assert!(matches!(&agent.history()[1], Message::Assistant { model, .. } if model == "mock/m1"));
}

#[tokio::test]
async fn step_limit_stops_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let script = (0..5).map(|i| Script::tool_call(&format!("c{i}"), "echo", json!({"text": "again"}))).collect();
    let provider = MockProvider::new(script);
    let mut agent = agent(provider.clone(), Mode::Auto, Arc::new(NonInteractive), dir.path());
    agent_config_max_steps(&mut agent, 3);
    let (reason, events) = run(&mut agent, "loop").await;
    assert_eq!(reason, TurnEndReason::StepLimit);
    assert_eq!(provider.requests().len(), 3);
    assert_eq!(events.last(), Some(&AgentEvent::TurnFinished { reason: TurnEndReason::StepLimit }));
}

fn agent_config_max_steps(agent: &mut harness_core::agent::Agent, steps: u32) {
    agent.config_mut().max_steps = steps;
}

#[tokio::test]
async fn invalid_json_arguments_are_not_executed_and_are_counted() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::raw_tool_call("c1", "touch", "{not json"), Script::text("sorry")]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    let outputs = finished_outputs(&events);
    assert!(outputs[0].1 && outputs[0].0.contains("not valid JSON"), "{outputs:?}");
    assert_eq!(agent.invalid_calls_this_turn(), 1);
}

#[tokio::test]
async fn schema_violations_are_reported_to_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::tool_call("c1", "echo", json!({"txt": "x"})), Script::text("ok")]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (_, events) = run(&mut agent, "go").await;
    let (output, is_error) = &finished_outputs(&events)[0];
    assert!(is_error);
    assert!(output.contains("invalid arguments") && output.contains("text"), "{output}");
}

#[tokio::test]
async fn unknown_tools_are_reported_to_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::tool_call("c1", "teleport", json!({})), Script::text("ok")]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (_, events) = run(&mut agent, "go").await;
    let (output, is_error) = &finished_outputs(&events)[0];
    assert!(is_error && output.contains("unknown tool `teleport`"));
}

#[tokio::test]
async fn headless_writes_in_ask_mode_are_blocked() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::tool_call("c1", "touch", json!({"path": "a.txt"})), Script::text("ok")]);
    let mut agent = agent(provider, Mode::Ask, Arc::new(NonInteractive), dir.path());
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert!(events.iter().any(|e| matches!(e, AgentEvent::ActionBlocked { id, .. } if id == "c1")));
    assert!(!dir.path().join("a.txt").exists());
}

#[tokio::test]
async fn approved_actions_run() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::tool_call("c1", "touch", json!({"path": "a.txt"})), Script::text("ok")]);
    let mut agent = agent(provider, Mode::Ask, Arc::new(AlwaysApprove), dir.path());
    let (_, events) = run(&mut agent, "go").await;
    assert!(events.iter().any(|e| matches!(e, AgentEvent::ApprovalNeeded { .. })));
    assert!(dir.path().join("a.txt").exists());
}

#[tokio::test]
async fn denied_actions_in_read_only_mode_never_ask() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::tool_call("c1", "touch", json!({"path": "a.txt"})), Script::text("ok")]);
    let mut agent = agent(provider, Mode::ReadOnly, Arc::new(AlwaysApprove), dir.path());
    let (_, events) = run(&mut agent, "go").await;
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::ApprovalNeeded { .. })));
    assert!(finished_outputs(&events)[0].0.starts_with("denied:"));
    assert!(!dir.path().join("a.txt").exists());
}

#[tokio::test]
async fn tool_failures_do_not_abort_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::tool_call("c1", "fail", json!({})), Script::text("recovered")]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(finished_outputs(&events), vec![("tool exploded".to_string(), true)]);
}

#[tokio::test]
async fn provider_errors_end_the_turn_but_the_session_stays_usable() {
    let dir = tempfile::tempdir().unwrap();
    let unauthorized = ProviderError::Http { status: 401, body: "bad key".into(), retry_after: None };
    let provider = MockProvider::new(vec![Script::error(unauthorized), Script::text("second try")]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (first, events) = run(&mut agent, "one").await;
    assert_eq!(first, TurnEndReason::Error);
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Error { message, .. } if message.contains("401"))));
    let (second, _) = run(&mut agent, "two").await;
    assert_eq!(second, TurnEndReason::Completed);
}

#[tokio::test]
async fn large_tool_output_is_spilled_to_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let big = "x".repeat(20_000);
    let provider = MockProvider::new(vec![Script::tool_call("c1", "echo", json!({"text": big})), Script::text("ok")]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (_, events) = run(&mut agent, "go").await;
    let (output, _) = &finished_outputs(&events)[0];
    assert!(output.contains("full output saved to"), "{}", &output[..200.min(output.len())]);
    assert!(output.len() < 12_000);
}
```

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo test -p harness-core --test agent`
Expected: FAIL to compile: `Agent`, `AgentConfig`, `NonInteractive`, and friends are not defined.

- [ ] **Step 5: Implement the agent loop**

`crates/harness-core/src/agent.rs`:

```rust
//! The agent loop: call the model, run the tools it requests, feed results back, repeat.

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::{
    event::{AgentEvent, ErrorKind, TurnEndReason},
    message::{ChatRequest, Message, ToolCall},
    output::{limit_output, DEFAULT_OUTPUT_LIMIT},
    permission::{Action, Decision, PermissionPolicy},
    provider::{FinishReason, Provider, ProviderError, ProviderEvent},
    tool::{ToolContext, ToolOutput, ToolRegistry},
};

/// Settings for one agent session.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// `<provider>/<model>`, recorded on every assistant message.
    pub model_id: String,
    /// The model name sent to the provider.
    pub model_name: String,
    pub system_prompt: String,
    pub max_steps: u32,
    pub output_limit: usize,
    pub output_dir: PathBuf,
}

impl AgentConfig {
    pub fn new(
        model_id: impl Into<String>,
        model_name: impl Into<String>,
        system_prompt: impl Into<String>,
        output_dir: PathBuf,
    ) -> Self {
        AgentConfig {
            model_id: model_id.into(),
            model_name: model_name.into(),
            system_prompt: system_prompt.into(),
            max_steps: 50,
            output_limit: DEFAULT_OUTPUT_LIMIT,
            output_dir,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub call_id: String,
    pub tool: String,
    pub action: Action,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approve,
    Deny { feedback: Option<String> },
    /// Nobody can answer (headless run): the action is blocked.
    Unavailable,
}

#[async_trait]
pub trait Approver: Send + Sync {
    async fn decide(&self, request: &ApprovalRequest) -> ApprovalDecision;
}

/// Used when nobody can answer prompts (e.g. `harness ask`): every approval is unavailable.
pub struct NonInteractive;

#[async_trait]
impl Approver for NonInteractive {
    async fn decide(&self, _request: &ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Unavailable
    }
}

/// What one model call produced so far. Kept outside the stream future so partial output survives.
#[derive(Debug, Default)]
struct ModelReply {
    text: String,
    tool_calls: Vec<ToolCall>,
    finish: Option<FinishReason>,
    /// Whether any output was already shown to the user (then the call must not be retried).
    emitted: bool,
}

pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
    policy: Arc<dyn PermissionPolicy>,
    approver: Arc<dyn Approver>,
    config: AgentConfig,
    ctx: ToolContext,
    history: Vec<Message>,
    validators: HashMap<String, jsonschema::Validator>,
    invalid_calls: u32,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: ToolRegistry,
        policy: Arc<dyn PermissionPolicy>,
        approver: Arc<dyn Approver>,
        config: AgentConfig,
        ctx: ToolContext,
    ) -> Self {
        let validators = tools
            .specs()
            .into_iter()
            .map(|spec| {
                let validator = jsonschema::validator_for(&spec.parameters)
                    .unwrap_or_else(|e| panic!("tool `{}` has an invalid schema: {e}", spec.name));
                (spec.name, validator)
            })
            .collect();
        Agent { provider, tools, policy, approver, config, ctx, history: Vec::new(), validators, invalid_calls: 0 }
    }

    pub fn history(&self) -> &[Message] {
        &self.history
    }

    pub fn config_mut(&mut self) -> &mut AgentConfig {
        &mut self.config
    }

    /// Invalid tool calls (unknown tool, bad JSON, schema violations) in the current or last turn.
    pub fn invalid_calls_this_turn(&self) -> u32 {
        self.invalid_calls
    }

    /// Runs one user turn to completion, reporting everything on `events`.
    pub async fn run_turn(
        &mut self,
        input: String,
        events: &UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
    ) -> TurnEndReason {
        self.ctx.cancel = cancel;
        self.invalid_calls = 0;
        let _ = events.send(AgentEvent::TurnStarted);
        self.history.push(Message::User { content: input });

        for _ in 0..self.config.max_steps {
            let mut reply = ModelReply::default();
            if let Err(error) = self.stream_into(&mut reply, events).await {
                return self.fail(error, reply, events);
            }
            let calls = std::mem::take(&mut reply.tool_calls);
            self.push_assistant(reply.text, calls.clone(), events);
            if calls.is_empty() {
                return self.finish(TurnEndReason::Completed, events);
            }
            for call in calls {
                let output = self.execute(&call, events).await;
                self.history.push(Message::Tool { call_id: call.id, content: output.content, is_error: output.is_error });
            }
        }
        self.finish(TurnEndReason::StepLimit, events)
    }

    fn push_assistant(&mut self, text: String, tool_calls: Vec<ToolCall>, events: &UnboundedSender<AgentEvent>) {
        let model = self.config.model_id.clone();
        let _ = events.send(AgentEvent::AssistantMessage { content: text.clone(), model: model.clone() });
        self.history.push(Message::Assistant { content: text, tool_calls, model });
    }

    fn finish(&self, reason: TurnEndReason, events: &UnboundedSender<AgentEvent>) -> TurnEndReason {
        let _ = events.send(AgentEvent::TurnFinished { reason });
        reason
    }

    fn fail(&mut self, error: ProviderError, partial: ModelReply, events: &UnboundedSender<AgentEvent>) -> TurnEndReason {
        if !partial.text.is_empty() {
            self.push_assistant(partial.text, Vec::new(), events);
        }
        let _ = events.send(AgentEvent::Error { kind: ErrorKind::Provider, message: describe(&error) });
        self.finish(TurnEndReason::Error, events)
    }

    /// Streams one model call into `reply`, forwarding deltas as events.
    async fn stream_into(&self, reply: &mut ModelReply, events: &UnboundedSender<AgentEvent>) -> Result<(), ProviderError> {
        let request = ChatRequest {
            model: self.config.model_name.clone(),
            system: self.config.system_prompt.clone(),
            messages: self.history.clone(),
            tools: self.tools.specs(),
        };
        let mut stream = self.provider.stream(request);
        while let Some(item) = stream.next().await {
            match item? {
                ProviderEvent::TextDelta(text) => {
                    reply.emitted = true;
                    reply.text.push_str(&text);
                    let _ = events.send(AgentEvent::TextDelta { text });
                }
                ProviderEvent::ReasoningDelta(text) => {
                    reply.emitted = true;
                    let _ = events.send(AgentEvent::ReasoningDelta { text });
                }
                ProviderEvent::ToolCall(call) => reply.tool_calls.push(call),
                ProviderEvent::Usage(usage) => {
                    let _ = events.send(AgentEvent::Usage { model: self.config.model_id.clone(), usage });
                }
                ProviderEvent::Finished(reason) => reply.finish = Some(reason),
            }
        }
        Ok(())
    }

    async fn execute(&mut self, call: &ToolCall, events: &UnboundedSender<AgentEvent>) -> ToolOutput {
        let _ = events.send(AgentEvent::ToolCallRequested {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        });
        let raw = self.execute_inner(call, events).await;
        let content = limit_output(&raw.content, self.config.output_limit, &self.config.output_dir, &call.id);
        let output = ToolOutput { content, is_error: raw.is_error };
        let _ = events.send(AgentEvent::ToolCallFinished {
            id: call.id.clone(),
            output: output.content.clone(),
            is_error: output.is_error,
        });
        output
    }

    async fn execute_inner(&mut self, call: &ToolCall, events: &UnboundedSender<AgentEvent>) -> ToolOutput {
        let Some(tool) = self.tools.get(&call.name) else {
            self.invalid_calls += 1;
            let names: Vec<String> = self.tools.specs().into_iter().map(|s| s.name).collect();
            return ToolOutput::error(format!("unknown tool `{}`; available tools: {}", call.name, names.join(", ")));
        };
        let raw = if call.arguments.trim().is_empty() { "{}" } else { call.arguments.as_str() };
        let args: Value = match serde_json::from_str(raw) {
            Ok(args) => args,
            Err(e) => {
                self.invalid_calls += 1;
                return ToolOutput::error(format!("arguments for `{}` are not valid JSON: {e}", call.name));
            }
        };
        let problems: Vec<String> = self
            .validators
            .get(&call.name)
            .map(|v| v.iter_errors(&args).map(|e| e.to_string()).collect())
            .unwrap_or_default();
        if !problems.is_empty() {
            self.invalid_calls += 1;
            return ToolOutput::error(format!("invalid arguments for `{}`: {}", call.name, problems.join("; ")));
        }

        let action = tool.action(&args, &self.ctx);
        match self.policy.check(&action) {
            Decision::Allow => {}
            Decision::Deny(reason) => return ToolOutput::error(format!("denied: {reason}")),
            Decision::Ask(reason) => {
                let _ = events.send(AgentEvent::ApprovalNeeded { id: call.id.clone(), reason: reason.clone() });
                let request =
                    ApprovalRequest { call_id: call.id.clone(), tool: call.name.clone(), action, reason: reason.clone() };
                match self.approver.decide(&request).await {
                    ApprovalDecision::Approve => {}
                    ApprovalDecision::Deny { feedback: Some(note) } => {
                        return ToolOutput::error(format!("the user denied this action: {note}"));
                    }
                    ApprovalDecision::Deny { feedback: None } => return ToolOutput::error("the user denied this action"),
                    ApprovalDecision::Unavailable => {
                        let _ = events.send(AgentEvent::ActionBlocked { id: call.id.clone(), reason: reason.clone() });
                        return ToolOutput::error(format!(
                            "blocked: {reason} needs approval and no user is available to approve it"
                        ));
                    }
                }
            }
        }
        tool.run(args, &self.ctx).await
    }
}

/// A human-readable error message for the user.
fn describe(error: &ProviderError) -> String {
    match error {
        ProviderError::Http { status: 429, .. } => {
            format!("{error}. The provider is rate limiting; try again later or switch models with --model.")
        }
        ProviderError::Http { status, body, .. } => {
            let body: String = body.chars().take(500).collect();
            format!("HTTP {status}: {body}")
        }
        other => other.to_string(),
    }
}
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p harness-core && cargo clippy -p harness-core --all-targets -- -D warnings`
Expected: all harness-core tests pass (13 in `agent.rs`); clippy clean. If clippy flags `ModelReply::finish` as never read, keep the field (P4 uses it for truncation detection) and add `#[allow(dead_code)] // read in P4 (truncation)` on that field only.

- [ ] **Step 7: Commit**

```bash
git add crates/harness-core
git commit -m "feat(core): add the agent loop with schema validation, approvals, and output spilling"
```

---

### Task 12: Retries and interruption

**Files:**
- Create: `crates/harness-core/src/retry.rs`
- Modify: `crates/harness-core/src/lib.rs`, `crates/harness-core/src/agent.rs`
- Test: `crates/harness-core/tests/resilience.rs`

**Interfaces:**
- Consumes: `Agent`, `AgentConfig`, `ModelReply`, `stream_into` (Task 11); `ProviderError::{is_retryable, retry_after}` (Task 2); `MockProvider`, `Script::Hang` (Task 11).
- Produces:
  - `harness_core::retry::RetryPolicy { max_attempts: u32 (default 5), base_delay: Duration (default 500 ms), max_delay: Duration (default 30 s) }` with `delay(&self, attempt: u32, retry_after: Option<Duration>) -> Duration`.
  - `AgentConfig.retry: RetryPolicy` (set by `AgentConfig::new` to the default).
  - `Agent::run_turn` now retries transient errors (emitting `Retrying`), and ends with `TurnEndReason::Interrupted` when `cancel` fires, keeping partial text and answering every pending tool call so history stays valid.

- [ ] **Step 1: Write the failing tests**

`crates/harness-core/tests/resilience.rs`:

```rust
mod common;

use std::{sync::Arc, time::Duration};

use common::*;
use harness_core::agent::NonInteractive;
use harness_core::event::{AgentEvent, TurnEndReason};
use harness_core::message::Message;
use harness_core::permission::Mode;
use harness_core::provider::{ProviderError, ProviderEvent};
use harness_core::retry::RetryPolicy;
use harness_core::testing::{MockProvider, Script};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn http(status: u16, retry_after: Option<Duration>) -> ProviderError {
    ProviderError::Http { status, body: String::new(), retry_after }
}

fn retries(events: &[AgentEvent]) -> Vec<(u32, u64)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Retrying { attempt, delay_ms, .. } => Some((*attempt, *delay_ms)),
            _ => None,
        })
        .collect()
}

#[test]
fn backoff_grows_is_capped_and_honours_retry_after() {
    let policy = RetryPolicy::default();
    assert_eq!(policy.delay(1, Some(Duration::from_secs(3))), Duration::from_secs(3));
    let first = policy.delay(1, None);
    assert!(first >= Duration::from_millis(500) && first < Duration::from_millis(1000));
    let third = policy.delay(3, None);
    assert!(third >= Duration::from_millis(2000) && third < Duration::from_millis(2500));
    assert!(policy.delay(20, None) < Duration::from_millis(30_500));
}

#[tokio::test(start_paused = true)]
async fn rate_limits_are_retried_after_the_requested_delay() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::error(http(429, Some(Duration::from_secs(3)))), Script::text("ok")]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let started = tokio::time::Instant::now();
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(retries(&events), vec![(1, 3000)]);
    assert!(started.elapsed() >= Duration::from_secs(3));
}

#[tokio::test(start_paused = true)]
async fn gives_up_after_five_attempts_and_the_session_stays_usable() {
    let dir = tempfile::tempdir().unwrap();
    let mut script: Vec<Script> = (0..5).map(|_| Script::error(http(503, None))).collect();
    script.push(Script::text("back"));
    let provider = MockProvider::new(script);
    let mut agent = agent(provider.clone(), Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Error);
    assert_eq!(retries(&events).len(), 4);
    assert_eq!(provider.requests().len(), 5);
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
    let (again, _) = run(&mut agent, "retry later").await;
    assert_eq!(again, TurnEndReason::Completed);
}

#[tokio::test(start_paused = true)]
async fn non_retryable_errors_are_not_retried() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::error(http(401, None))]);
    let mut agent = agent(provider.clone(), Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Error);
    assert!(retries(&events).is_empty());
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn errors_after_streamed_output_are_not_retried_and_keep_the_partial_text() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::Reply(vec![
        Ok(ProviderEvent::TextDelta("partial".into())),
        Err(ProviderError::Network("reset".into())),
    ])]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Error);
    assert!(retries(&events).is_empty());
    assert!(matches!(&agent.history()[1], Message::Assistant { content, .. } if content == "partial"));
}

#[tokio::test(start_paused = true)]
async fn interrupt_during_the_model_stream_keeps_partial_output() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::Hang(vec![ProviderEvent::TextDelta("thinking".into())])]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        trigger.cancel();
    });
    let (reason, events) = run_with(&mut agent, "go", cancel).await;
    assert_eq!(reason, TurnEndReason::Interrupted);
    assert_eq!(events.last(), Some(&AgentEvent::TurnFinished { reason: TurnEndReason::Interrupted }));
    assert!(matches!(&agent.history()[1], Message::Assistant { content, .. } if content == "thinking"));
}

#[tokio::test(start_paused = true)]
async fn interrupt_during_a_tool_answers_every_pending_call() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::Reply(vec![
            Ok(ProviderEvent::ToolCall(harness_core::message::ToolCall { id: "a".into(), name: "sleepy".into(), arguments: "{}".into() })),
            Ok(ProviderEvent::ToolCall(harness_core::message::ToolCall { id: "b".into(), name: "echo".into(), arguments: r#"{"text":"x"}"#.into() })),
        ]),
        Script::text("must not be requested"),
    ]);
    let mut agent = agent(provider.clone(), Mode::Auto, Arc::new(NonInteractive), dir.path());
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        trigger.cancel();
    });
    let (reason, _) = run_with(&mut agent, "go", cancel).await;
    assert_eq!(reason, TurnEndReason::Interrupted);
    assert_eq!(provider.requests().len(), 1);
    let tool_results: Vec<&str> = agent
        .history()
        .iter()
        .filter_map(|m| match m {
            Message::Tool { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results, ["a", "b"], "every tool call must have a result");
}

#[tokio::test(start_paused = true)]
async fn interrupt_during_backoff_stops_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::error(http(429, Some(Duration::from_secs(600))))]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        trigger.cancel();
    });
    let started = tokio::time::Instant::now();
    let (reason, _) = run_with(&mut agent, "go", cancel).await;
    assert_eq!(reason, TurnEndReason::Interrupted);
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test(start_paused = true)]
async fn tool_calls_still_run_normally_without_interrupts() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::tool_call("c1", "echo", json!({"text": "fine"})), Script::text("ok")]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(finished_outputs(&events), vec![("fine".to_string(), false)]);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p harness-core --test resilience`
Expected: FAIL to compile: `harness_core::retry` does not exist.

- [ ] **Step 3: Implement the retry policy**

Add `pub mod retry;` to `crates/harness-core/src/lib.rs` (keep the list alphabetical, after `provider`).

`crates/harness-core/src/retry.rs`:

```rust
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How transient provider errors (network, 429, 5xx) are retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total attempts including the first.
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy { max_attempts: 5, base_delay: Duration::from_millis(500), max_delay: Duration::from_secs(30) }
    }
}

impl RetryPolicy {
    /// Delay before retry number `attempt` (1-based). A server-provided `Retry-After` wins; otherwise
    /// exponential backoff capped at `max_delay`, plus up to `base_delay` of jitter.
    pub fn delay(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        if let Some(requested) = retry_after {
            return requested;
        }
        let exponent = attempt.saturating_sub(1).min(16);
        let backoff = self.base_delay.saturating_mul(1u32 << exponent).min(self.max_delay);
        backoff + jitter(self.base_delay)
    }
}

fn jitter(max: Duration) -> Duration {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0) as u64;
    let span = (max.as_millis() as u64).max(1);
    Duration::from_millis(nanos % span)
}
```

- [ ] **Step 4: Wire retries and cancellation into the agent**

In `crates/harness-core/src/agent.rs`:

1. Add `retry::RetryPolicy,` to the `use crate::{ ... }` list.
2. Add the field `pub retry: RetryPolicy,` to `AgentConfig` (after `output_dir`) and `retry: RetryPolicy::default(),` to the struct literal in `AgentConfig::new`.
3. Add this enum next to `ModelReply`:

```rust
/// How one model call (including its retries) ended.
enum ModelOutcome {
    Reply(ModelReply),
    Failed(ProviderError, ModelReply),
    Interrupted(ModelReply),
}
```

4. Replace `run_turn` with:

```rust
    /// Runs one user turn to completion, reporting everything on `events`. Cancelling `cancel` stops the
    /// turn promptly: in-flight model calls are dropped and running tools are told to stop.
    pub async fn run_turn(
        &mut self,
        input: String,
        events: &UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
    ) -> TurnEndReason {
        self.ctx.cancel = cancel.clone();
        self.invalid_calls = 0;
        let _ = events.send(AgentEvent::TurnStarted);
        self.history.push(Message::User { content: input });

        for _ in 0..self.config.max_steps {
            let reply = match self.call_model(events, &cancel).await {
                ModelOutcome::Reply(reply) => reply,
                ModelOutcome::Failed(error, partial) => return self.fail(error, partial, events),
                ModelOutcome::Interrupted(partial) => {
                    if !partial.text.is_empty() {
                        self.push_assistant(partial.text, Vec::new(), events);
                    }
                    return self.finish(TurnEndReason::Interrupted, events);
                }
            };
            let calls = reply.tool_calls.clone();
            self.push_assistant(reply.text, calls.clone(), events);
            if calls.is_empty() {
                return self.finish(TurnEndReason::Completed, events);
            }
            for (index, call) in calls.iter().enumerate() {
                if cancel.is_cancelled() {
                    // Every tool call needs a result, or the next request would be rejected.
                    for skipped in &calls[index..] {
                        self.history.push(Message::Tool {
                            call_id: skipped.id.clone(),
                            content: "interrupted by the user before this tool ran".into(),
                            is_error: true,
                        });
                    }
                    return self.finish(TurnEndReason::Interrupted, events);
                }
                let output = self.execute(call, events).await;
                self.history.push(Message::Tool {
                    call_id: call.id.clone(),
                    content: output.content,
                    is_error: output.is_error,
                });
            }
            if cancel.is_cancelled() {
                return self.finish(TurnEndReason::Interrupted, events);
            }
        }
        self.finish(TurnEndReason::StepLimit, events)
    }

    /// One model call with retries for transient errors. Never retries once output reached the user.
    async fn call_model(&self, events: &UnboundedSender<AgentEvent>, cancel: &CancellationToken) -> ModelOutcome {
        let mut attempt = 1;
        loop {
            let mut reply = ModelReply::default();
            let result = tokio::select! {
                result = self.stream_into(&mut reply, events) => Some(result),
                _ = cancel.cancelled() => None,
            };
            match result {
                None => return ModelOutcome::Interrupted(reply),
                Some(Ok(())) => return ModelOutcome::Reply(reply),
                Some(Err(error))
                    if error.is_retryable() && !reply.emitted && attempt < self.config.retry.max_attempts =>
                {
                    let delay = self.config.retry.delay(attempt, error.retry_after());
                    let _ = events.send(AgentEvent::Retrying {
                        attempt,
                        reason: error.to_string(),
                        delay_ms: delay.as_millis() as u64,
                    });
                    let waited = tokio::select! {
                        _ = tokio::time::sleep(delay) => true,
                        _ = cancel.cancelled() => false,
                    };
                    if !waited {
                        return ModelOutcome::Interrupted(ModelReply::default());
                    }
                    attempt += 1;
                }
                Some(Err(error)) => return ModelOutcome::Failed(error, reply),
            }
        }
    }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p harness-core && cargo clippy -p harness-core --all-targets -- -D warnings`
Expected: all harness-core tests pass, including the 9 in `resilience.rs` and the 13 in `agent.rs`; clippy clean.

- [ ] **Step 6: Commit**

```bash
git add crates/harness-core
git commit -m "feat(core): retry transient provider errors and support interrupting turns"
```

---

### Task 13: `harness ask` and `harness models`

**Files:**
- Modify: `crates/harness-cli/Cargo.toml`, `crates/harness-cli/src/main.rs`
- Create: `crates/harness-cli/src/setup.rs`, `src/prompt.rs`, `src/ask.rs`, `src/models.rs`
- Test: `crates/harness-cli/tests/ask_e2e.rs` (plus unit tests inside `src/prompt.rs` and `src/ask.rs`)

**Interfaces:**
- Consumes: `Paths`, `config::load` (Task 3); `resolve`, `local_endpoints`, `configured_endpoints`, `list_models`, `LOCAL_PROBE_TIMEOUT`, `REMOTE_PROBE_TIMEOUT` (Task 10); `builtin()` (Task 8); `Agent`, `AgentConfig`, `NonInteractive` (Tasks 11–12); `BaselinePolicy`, `Mode` (Tasks 2, 4); `ToolContext` (Task 5); `AgentEvent`, `TurnEndReason` (Task 2).
- Produces: the `harness` CLI: `harness [--model <provider>/<model>] [--mode <mode>] ask [--json] <prompt>...` and `harness models`; `ask::exit_code(TurnEndReason, blocked: bool) -> u8`; `prompt::system_prompt(workspace: &Path, date: &str) -> String`; `prompt::civil_date(unix_secs: u64) -> String`.

- [ ] **Step 1: Update the manifest**

`crates/harness-cli/Cargo.toml`:

```toml
[package]
name = "harness-cli"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[[bin]]
name = "harness"
path = "src/main.rs"

[dependencies]
clap.workspace = true
harness-config.workspace = true
harness-core.workspace = true
harness-providers.workspace = true
harness-tools.workspace = true
serde_json.workspace = true
tokio.workspace = true
tokio-util.workspace = true

[dev-dependencies]
assert_cmd.workspace = true
nix.workspace = true
predicates.workspace = true
serde_json.workspace = true
tempfile.workspace = true
tokio.workspace = true
wiremock.workspace = true
```

- [ ] **Step 2: Write the failing end-to-end tests**

`crates/harness-cli/tests/ask_e2e.rs`:

```rust
use std::process::Stdio;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use predicates::str::contains;
use serde_json::{json, Value};
use tempfile::TempDir;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_harness");

fn sse(chunks: &[Value]) -> String {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    body
}

fn text_chunk(text: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": "stop"}]})
}

fn tool_chunk(id: &str, name: &str, arguments: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": id, "type": "function",
        "function": {"name": name, "arguments": arguments}}]}, "finish_reason": "tool_calls"}]})
}

fn stream(chunks: &[Value]) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(sse(chunks), "text/event-stream")
}

struct Env {
    home: TempDir,
    ws: TempDir,
}

impl Env {
    /// A HARNESS_HOME whose config defines provider `mock` at `server_uri`, and a workspace that looks
    /// like a git work tree (so the default mode is `auto`).
    fn new(server_uri: &str, extra_config: &str) -> Env {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(
            home.path().join("config/config.toml"),
            format!("{extra_config}\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{server_uri}/v1\"\n"),
        )
        .unwrap();
        std::fs::create_dir(ws.path().join(".git")).unwrap();
        Env { home, ws }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.current_dir(self.ws.path())
            .env("HARNESS_HOME", self.home.path())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME");
        cmd
    }
}

async fn write_then_answer(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains("\"role\":\"tool\""))
        .respond_with(stream(&[text_chunk("Created hello.txt")]))
        .with_priority(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(stream(&[tool_chunk("c1", "write", r#"{"path":"hello.txt","content":"hi\n"}"#)]))
        .with_priority(2)
        .mount(server)
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ask_runs_a_multi_step_task_and_prints_the_final_answer() {
    let server = MockServer::start().await;
    write_then_answer(&server).await;
    let env = Env::new(&server.uri(), "");
    let env = tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["--model", "mock/test-model", "ask", "make", "hello.txt"])
            .assert()
            .success()
            .stdout(contains("Created hello.txt"));
        env
    })
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(env.ws.path().join("hello.txt")).unwrap(), "hi\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn json_output_is_one_event_per_line_ending_with_turn_finished() {
    let server = MockServer::start().await;
    write_then_answer(&server).await;
    let env = Env::new(&server.uri(), "model = \"mock/test-model\"");
    let output = tokio::task::spawn_blocking(move || env.cmd().args(["ask", "--json", "go"]).output().unwrap())
        .await
        .unwrap();
    assert!(output.status.success());
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).expect("every stdout line is JSON"))
        .collect();
    assert_eq!(events.first().unwrap()["type"], "turn_started");
    assert_eq!(events.last().unwrap()["type"], "turn_finished");
    assert_eq!(events.last().unwrap()["reason"], "completed");
}

#[tokio::test(flavor = "multi_thread")]
async fn ask_mode_blocks_writes_and_exits_3() {
    let server = MockServer::start().await;
    write_then_answer(&server).await;
    let env = Env::new(&server.uri(), "model = \"mock/test-model\"");
    let env = tokio::task::spawn_blocking(move || {
        env.cmd().args(["--mode", "ask", "ask", "go"]).assert().code(3).stderr(contains("blocked"));
        env
    })
    .await
    .unwrap();
    assert!(!env.ws.path().join("hello.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn piped_stdin_is_appended_to_the_prompt() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("review this"))
        .and(body_string_contains("diff --git a/x b/x"))
        .respond_with(stream(&[text_chunk("looks fine")]))
        .mount(&server)
        .await;
    let env = Env::new(&server.uri(), "model = \"mock/test-model\"");
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["ask", "review this"])
            .write_stdin("diff --git a/x b/x\n")
            .assert()
            .success()
            .stdout(contains("looks fine"));
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_errors_exit_1() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(401).set_body_string("bad key")).mount(&server).await;
    let env = Env::new(&server.uri(), "model = \"mock/test-model\"");
    tokio::task::spawn_blocking(move || {
        env.cmd().args(["ask", "hi"]).assert().code(1).stderr(contains("HTTP 401"));
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_model_and_unknown_provider_exit_2() {
    let server = MockServer::start().await;
    let env = Env::new(&server.uri(), "");
    tokio::task::spawn_blocking(move || {
        env.cmd().args(["ask", "hi"]).assert().code(2).stderr(contains("no model configured"));
        env.cmd().args(["--model", "nope/x", "ask", "hi"]).assert().code(2).stderr(contains("unknown provider"));
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_config_exits_2_with_the_file_and_line() {
    let server = MockServer::start().await;
    let env = Env::new(&server.uri(), "mdoe = \"auto\"");
    tokio::task::spawn_blocking(move || {
        env.cmd().args(["ask", "hi"]).assert().code(2).stderr(contains("config.toml")).stderr(contains("mdoe"));
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn models_lists_configured_provider_models() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "m1"}]})))
        .mount(&server)
        .await;
    let env = Env::new(&server.uri(), "");
    tokio::task::spawn_blocking(move || {
        env.cmd().arg("models").assert().success().stdout(contains("mock/m1"));
    })
    .await
    .unwrap();
}

// Review Focus: Ctrl+C during `harness ask`.
#[tokio::test(flavor = "multi_thread")]
async fn ctrl_c_interrupts_the_run_and_exits_130() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(stream(&[text_chunk("too late")]).set_delay(Duration::from_secs(30)))
        .mount(&server)
        .await;
    let env = Env::new(&server.uri(), "model = \"mock/test-model\"");
    let (code, elapsed) = tokio::task::spawn_blocking(move || {
        let mut child = std::process::Command::new(BIN)
            .args(["ask", "hi"])
            .current_dir(env.ws.path())
            .env("HARNESS_HOME", env.home.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(800));
        let started = Instant::now();
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(child.id() as i32), nix::sys::signal::Signal::SIGINT).unwrap();
        let status = child.wait().unwrap();
        (status.code(), started.elapsed())
    })
    .await
    .unwrap();
    assert_eq!(code, Some(130));
    assert!(elapsed < Duration::from_secs(5));
}

#[test]
fn no_subcommand_explains_that_interactive_mode_is_not_ready() {
    Command::new(BIN).assert().code(2).stderr(contains("harness ask"));
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p harness-cli --test ask_e2e`
Expected: FAIL: the tests compile, but every assertion fails because `harness` has no `ask` or `models` subcommand (clap exits 2 with "unrecognized subcommand").

- [ ] **Step 4: Implement the prompt module (with its unit tests)**

`crates/harness-cli/src/prompt.rs`:

```rust
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

/// The base system prompt plus environment facts captured once per run. Kept short on purpose:
/// local models have small context windows. P3 replaces this with full context assembly.
pub fn system_prompt(workspace: &Path, date: &str) -> String {
    format!(
        "You are harness, a coding agent working in the user's project.\n\
         Use the tools to inspect files and make changes; never guess file contents.\n\
         Read a file before editing it. Make focused changes, and verify them (for example by running the tests) when you can.\n\
         When you are done, reply with a short summary of what you changed.\n\
         \n\
         Working directory: {}\n\
         Operating system: {}\n\
         Date: {date}\n",
        workspace.display(),
        std::env::consts::OS
    )
}

/// Today's date in UTC as `YYYY-MM-DD`.
pub fn today_utc() -> String {
    civil_date(SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0))
}

/// Converts Unix seconds to a UTC calendar date (Howard Hinnant's days-to-civil algorithm).
pub fn civil_date(unix_secs: u64) -> String {
    let z = (unix_secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_are_correct() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(1_709_164_800), "2024-02-29");
        assert_eq!(civil_date(1_790_208_000), "2026-09-24");
    }

    #[test]
    fn base_prompt_is_under_1000_tokens() {
        let prompt = system_prompt(Path::new("/some/project"), "2026-09-24");
        assert!(prompt.len() / 4 < 1000, "~{} tokens", prompt.len() / 4);
        assert!(prompt.contains("Working directory: /some/project"));
    }
}
```

- [ ] **Step 5: Implement setup, ask, models, and main**

`crates/harness-cli/src/setup.rs`:

```rust
use std::path::{Path, PathBuf};

use harness_config::{
    config::{self, Config},
    paths::Paths,
};
use harness_core::permission::Mode;

/// Everything a command needs about where it runs.
pub struct Setup {
    pub paths: Paths,
    pub config: Config,
    pub workspace: PathBuf,
}

/// Loads paths and configuration. Errors are user-facing messages (exit code 2).
pub fn load() -> Result<Setup, String> {
    let workspace = std::env::current_dir()
        .and_then(|dir| dir.canonicalize())
        .map_err(|e| format!("cannot determine the working directory: {e}"))?;
    let paths = Paths::from_process_env().map_err(|e| e.to_string())?;
    let config = config::load(&paths.global_config_file(), &workspace).map_err(|e| e.to_string())?;
    for warning in &config.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(Setup { paths, config, workspace })
}

/// `auto` inside a git work tree (changes are recoverable), `ask` elsewhere.
pub fn default_mode(workspace: &Path) -> Mode {
    if workspace.ancestors().any(|dir| dir.join(".git").exists()) { Mode::Auto } else { Mode::Ask }
}

pub fn env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}
```

`crates/harness-cli/src/models.rs`:

```rust
use harness_providers::{
    discovery::{self, DiscoveredModel, LOCAL_PROBE_TIMEOUT, REMOTE_PROBE_TIMEOUT},
    registry,
};

use crate::setup::{self, Setup};

/// Models from local servers and configured providers, local first.
pub async fn available(setup: &Setup) -> Vec<DiscoveredModel> {
    let local = registry::local_endpoints(&setup.config.providers);
    let configured = registry::configured_endpoints(&setup.config.providers, setup::env);
    let (mut found, remote) = tokio::join!(
        discovery::list_models(&local, LOCAL_PROBE_TIMEOUT),
        discovery::list_models(&configured, REMOTE_PROBE_TIMEOUT)
    );
    found.extend(remote);
    found
}

pub async fn run() -> u8 {
    let setup = match setup::load() {
        Ok(setup) => setup,
        Err(message) => {
            eprintln!("error: {message}");
            return 2;
        }
    };
    let found = available(&setup).await;
    if found.is_empty() {
        eprintln!(
            "No models found. Start Ollama, LM Studio, or llama.cpp, or configure a provider in {}.",
            setup.paths.global_config_file().display()
        );
    }
    for model in found {
        println!("{}", model.id());
    }
    0
}
```

`crates/harness-cli/src/ask.rs`:

```rust
use std::{
    io::{IsTerminal, Read},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    event::{AgentEvent, TurnEndReason},
    permission::{BaselinePolicy, Mode},
    tool::ToolContext,
};
use harness_providers::registry;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{models, prompt, setup};

pub async fn run(model_flag: Option<String>, mode_flag: Option<Mode>, prompt_text: String, json: bool) -> u8 {
    let setup = match setup::load() {
        Ok(setup) => setup,
        Err(message) => {
            eprintln!("error: {message}");
            return 2;
        }
    };
    let input = with_piped_stdin(prompt_text);

    let Some(model_id) = model_flag.or_else(|| setup.config.model.clone()) else {
        eprintln!("error: no model configured.");
        let found = models::available(&setup).await;
        if found.is_empty() {
            eprintln!("No local model servers were found. Start Ollama, LM Studio, or llama.cpp, or configure a provider.");
        } else {
            eprintln!("Available models:");
            for model in &found {
                eprintln!("  {}", model.id());
            }
        }
        eprintln!(
            "Pass one with --model, or set `model = \"<provider>/<model>\"` in {}.",
            setup.paths.global_config_file().display()
        );
        return 2;
    };
    let resolved = match registry::resolve(&model_id, &setup.config.providers, setup::env) {
        Ok(resolved) => resolved,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };

    let mode = mode_flag.or(setup.config.mode).unwrap_or_else(|| setup::default_mode(&setup.workspace));
    let run_id = format!(
        "run-{}-{}",
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        std::process::id()
    );
    let output_dir = setup.paths.state_dir.join("tool-output").join(run_id);
    let policy = Arc::new(BaselinePolicy::new(mode, &setup.workspace, vec![output_dir.clone()]));
    let mut config = AgentConfig::new(
        resolved.id.clone(),
        resolved.model.clone(),
        prompt::system_prompt(&setup.workspace, &prompt::today_utc()),
        output_dir,
    );
    if let Some(steps) = setup.config.max_steps {
        config.max_steps = steps;
    }
    let mut agent = Agent::new(
        resolved.provider,
        harness_tools::builtin(),
        policy,
        Arc::new(NonInteractive),
        config,
        ToolContext::new(&setup.workspace),
    );

    let cancel = CancellationToken::new();
    let on_ctrl_c = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            on_ctrl_c.cancel();
        }
    });

    let (tx, rx) = mpsc::unbounded_channel();
    let renderer = tokio::spawn(render(rx, json));
    let reason = agent.run_turn(input, &tx, cancel).await;
    drop(tx);
    let (final_text, blocked) = renderer.await.unwrap_or_default();
    if !json && !final_text.is_empty() {
        println!("{final_text}");
    }
    exit_code(reason, blocked)
}

/// Maps how the turn ended to the documented exit codes.
pub fn exit_code(reason: TurnEndReason, blocked: bool) -> u8 {
    match reason {
        TurnEndReason::Completed if blocked => 3,
        TurnEndReason::Completed => 0,
        TurnEndReason::Interrupted => 130,
        TurnEndReason::StepLimit | TurnEndReason::Error => 1,
    }
}

fn with_piped_stdin(prompt_text: String) -> String {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return prompt_text;
    }
    let mut piped = String::new();
    if stdin.lock().read_to_string(&mut piped).is_ok() && !piped.trim().is_empty() {
        format!("{prompt_text}\n\n{piped}")
    } else {
        prompt_text
    }
}

/// Prints events as they arrive. Returns the last assistant text and whether an action was blocked.
async fn render(mut rx: mpsc::UnboundedReceiver<AgentEvent>, json: bool) -> (String, bool) {
    let mut last_text = String::new();
    let mut blocked = false;
    while let Some(event) = rx.recv().await {
        if json {
            println!("{}", serde_json::to_string(&event).expect("events serialize"));
        }
        match &event {
            AgentEvent::AssistantMessage { content, .. } if !content.is_empty() => last_text = content.clone(),
            AgentEvent::ActionBlocked { reason, .. } => {
                blocked = true;
                if !json {
                    eprintln!("blocked: {reason}");
                }
            }
            AgentEvent::ToolCallRequested { name, arguments, .. } if !json => {
                let shown: String = arguments.chars().take(120).collect();
                eprintln!("-> {name} {shown}");
            }
            AgentEvent::Retrying { attempt, reason, delay_ms } if !json => {
                eprintln!("retrying (attempt {attempt}) in {delay_ms} ms: {reason}");
            }
            AgentEvent::Error { message, .. } if !json => eprintln!("error: {message}"),
            AgentEvent::TurnFinished { reason: TurnEndReason::StepLimit } if !json => {
                eprintln!("error: stopped after reaching the step limit");
            }
            _ => {}
        }
    }
    (last_text, blocked)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_the_spec() {
        assert_eq!(exit_code(TurnEndReason::Completed, false), 0);
        assert_eq!(exit_code(TurnEndReason::Completed, true), 3);
        assert_eq!(exit_code(TurnEndReason::Error, false), 1);
        assert_eq!(exit_code(TurnEndReason::StepLimit, false), 1);
        assert_eq!(exit_code(TurnEndReason::Interrupted, false), 130);
    }
}
```

`crates/harness-cli/src/main.rs`:

```rust
mod ask;
mod models;
mod prompt;
mod setup;

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use harness_core::permission::Mode;

#[derive(Parser)]
#[command(name = "harness", version, about = "A hybrid local/frontier coding agent")]
struct Cli {
    /// Model to use, as <provider>/<model> (e.g. ollama/qwen3-coder:30b)
    #[arg(long, global = true)]
    model: Option<String>,
    /// Approval mode: plan, read-only, ask, auto, or full-access
    #[arg(long, global = true)]
    mode: Option<Mode>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run one prompt to completion without interaction; piped stdin is appended to the prompt
    Ask {
        /// Print every event as one JSON object per line
        #[arg(long)]
        json: bool,
        /// The prompt
        #[arg(required = true, num_args = 1..)]
        prompt: Vec<String>,
    },
    /// List models from local servers and configured providers
    Models,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Runtime::new().expect("failed to start the tokio runtime");
    let code = runtime.block_on(async move {
        match cli.command {
            Some(Command::Ask { json, prompt }) => ask::run(cli.model, cli.mode, prompt.join(" "), json).await,
            Some(Command::Models) => models::run().await,
            None => {
                eprintln!("Interactive mode is not available yet; use `harness ask \"...\"`.");
                2
            }
        }
    });
    ExitCode::from(code)
}
```

- [ ] **Step 6: Run the whole suite and the gates**

Run: `cargo test --workspace && cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: every test passes, including the 10 in `ask_e2e.rs`, the 2 prompt unit tests, the exit-code unit test, and `cli_smoke`; fmt and clippy are clean.

Then check dependencies: `cargo install cargo-deny --locked` (once), then `cargo deny check`.
Expected: `advisories ok, bans ok, licenses ok, sources ok`. If a dependency uses a permissive licence missing from `deny.toml`'s allow list, add that exact SPDX id and note it in the commit message. Do not allow copyleft licences (GPL, AGPL, LGPL).

- [ ] **Step 7: Try it against a real local model (manual check)**

If Ollama is running with a tool-capable model (e.g. `qwen3:14b`):

Run: `mkdir -p /tmp/harness-demo && cd /tmp/harness-demo && git init -q && cargo run -q -p harness-cli -- --model ollama/qwen3:14b ask "create hello.py that prints hello, then tell me what you did"`
Expected: `-> write {"path":"hello.py",...}` on stderr, a summary on stdout, and `hello.py` exists. This check is informational: small local models may answer badly, which P4's model profiles address. Note in the task report what happened.

- [ ] **Step 8: Commit**

```bash
git add crates/harness-cli Cargo.lock deny.toml
git commit -m "feat(cli): add harness ask (plain/NDJSON, exit codes, Ctrl+C) and harness models"
```

---

## Plan Completion Checklist

After Task 13, verify the P1 slice of the spec end to end:

- [ ] `cargo nextest run --workspace` (or `cargo test --workspace`) passes on macOS; CI passes on macOS and Ubuntu.
- [ ] `harness models` lists your local Ollama models.
- [ ] `git diff | harness ask "summarize this diff"` prints a summary.
- [ ] `harness ask --json "list the files here"` prints NDJSON ending in `turn_finished`.
- [ ] Update `openspec/changes/add-core-agent/tasks.md`: tick group 1 (P1).
