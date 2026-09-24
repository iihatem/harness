# Proposal: add-core-agent (Milestone 1)

## Why

Developers increasingly pay for several AI subscriptions (ChatGPT, Claude) and also run capable local models, but every coding agent locks them into one vendor's loop, so they burn frontier usage on work a local model could do. `harness` is a public, open-source, terminal-first coding agent written in Rust whose long-term goal is hybrid development: mix local models, API keys, and subscriptions (within each vendor's terms) to get work done while saving usage.

Research across 30+ coding agents (2026-09-24; see design.md References) shows developers stay for correctness and trust, and leave over opaque cost, destructive accidents, and silently changing behaviour. Milestone 1 builds the core that every later capability plugs into, including the table-stakes features whose absence makes users switch tools: plan mode, rewind, safe permissions, and first-class local-model support.

## What Changes

- New Rust workspace producing a single `harness` binary for macOS and Linux, licensed MIT OR Apache-2.0, with no telemetry.
- Agent runtime with a typed event stream, multi-step tool use, mid-turn steering, interruption, retries, and error recovery.
- Model providers over three wire protocols (OpenAI Chat Completions, OpenAI Responses, Anthropic Messages), automatic discovery of local servers (Ollama, LM Studio, llama.cpp), mid-session model switching, and per-model profiles that make local models reliable (effective-context detection, text tool-call parsing, truncation detection).
- Credentials: API keys in the OS keychain, ChatGPT sign-in (browser and device-code flows), and multiple named accounts per provider. Claude subscription credentials are deliberately not supported; Claude subscriptions arrive in M3 via delegation to the official `claude` binary.
- Six built-in tools: `read`, `write`, `edit`, `bash`, `grep`, `glob`; large tool output is saved to a file with a preview sent to the model.
- Approval modes (`plan`, `read-only`, `ask`, `auto`, `full-access`), allow/deny rules that cannot be bypassed with compound commands, always-confirm destructive commands, approval for reading outside the workspace, and an OS sandbox for shell commands (Seatbelt on macOS, Landlock + seccomp on Linux).
- Plan mode: read-only exploration that ends in a plan the user can edit and approve before building.
- Checkpoints: per-turn workspace snapshots and `/rewind` to restore code, conversation, or both.
- Project instructions from `AGENTS.md` / `CLAUDE.md` with `@file` imports, assembled into a cache-stable prompt prefix.
- Slash commands: built-ins plus Markdown custom commands compatible with `.claude/commands` and `.opencode/commands` (so existing commands such as OpenSpec's `/opsx:*` work unchanged).
- Persistent, resumable, branchable sessions with automatic compaction.
- XDG-compliant configuration with workspace trust for project-level settings.
- Interactive inline terminal UI (native scrollback, `/context` breakdown, per-turn speed and cache stats, desktop notifications, collapsed pastes) plus headless `harness ask` with plain or NDJSON output and documented exit codes.

### Roadmap (not in this change)

| Milestone | Scope |
|---|---|
| M2 Routing, usage, verification | Model roles (plan, build, explore, background); model switches only at boundaries (plan→build handoff, subagent start, `/escalate`, announced fallback chains on rate-limit/quota/overflow, suggested escalation after repeated failures), never silently and never per-request; outcome logging for a future data-driven router; usage ledger with subscription windows, "$ saved", budgets, auto-resume after quota reset; verification gates (configured test/lint commands before "done"); LSP diagnostics |
| M3 Agents | Subagents with per-subagent models; Claude Code (`claude -p` stream-json / ACP) and Codex (`codex app-server`) as delegated backends; parallel agents in git worktrees with setup scripts and port ranges; background agents with `harness agents` status; best-of-N across backends |
| M4 Ecosystem | Hooks, MCP client (lazy tool loading), Agent Skills (`SKILL.md`), ACP server for editors, custom statusline, image input |
| M5 Distribution | Homebrew, install script, release pipeline, docs site, native Windows, remote/phone steering, scheduled runs |

## Capabilities

### New Capabilities

- `agent-runtime`: The agent loop, its event stream, steering, interruption, retries, tool-failure handling, model attribution, and the no-telemetry guarantee.
- `model-providers`: Wire-protocol adapters, model identifiers, local server discovery, model switching, model profiles, tool-call parsing and validation, and truncation handling.
- `provider-auth`: Credential sources and storage, ChatGPT sign-in, account profiles, logout, secret redaction, and the Claude-credential prohibition.
- `builtin-tools`: Behaviour of the `read`, `write`, `edit`, `bash`, `grep`, and `glob` tools, including large-output handling.
- `permissions-sandbox`: Approval modes, allow/deny rules, compound-command handling, destructive-command confirmation, approval prompts, path boundaries, and OS sandboxing of shell commands.
- `plan-mode`: Read-only planning that produces a plan the user can edit and approve before implementation.
- `checkpoints`: Per-turn workspace snapshots and rewinding code and conversation.
- `project-context`: Discovery and loading of `AGENTS.md` / `CLAUDE.md` instructions, environment information, and cache-stable prompt assembly.
- `slash-commands`: Built-in commands and Markdown custom commands (discovery, namespacing, frontmatter, placeholders).
- `sessions`: Session persistence, resumption, branching, crash tolerance, and compaction.
- `configuration`: Configuration file locations (XDG), precedence, and workspace trust for project-level settings.
- `cli-interface`: The interactive inline terminal UI, the headless `ask` mode, output formats, and exit codes.

### Modified Capabilities

None (greenfield project).

## Impact

- Creates the entire codebase: a Cargo workspace, CI (macOS + Linux), and licence files.
- Writes user data under XDG directories (`~/.config/harness`, `~/.local/share/harness`, `~/.local/state/harness`) and credentials to the OS keychain.
- Makes network calls only to configured providers and to localhost for local-server discovery; sends no telemetry.
- Requires `git` on `PATH` for checkpoints (degrades with a warning when absent).
- Depends on third-party crates (`tokio`, `reqwest`, `ratatui`, `crossterm`, `clap`, `keyring`, ripgrep's `ignore`/`grep`, and others), gated by `cargo-deny`.
- Policy dependency: ChatGPT sign-in relies on OpenAI's current, publicly stated tolerance of third-party harnesses; see design.md Risks.
