# harness

A terminal-first, open-source coding agent written in Rust, built for **hybrid development**: mix local models with frontier models so you get work done while saving subscription usage.

> **Status: early development.** Milestone 1, phase P1 (foundation) is complete: a headless `harness ask` that runs multi-step coding tasks against any OpenAI-compatible model. Safety sandboxing, sessions, more providers, and the interactive terminal UI are in progress. Not ready for daily use yet.

## What works today

- `harness ask "<prompt>"`: runs one task to completion with six tools (`read`, `write`, `edit`, `bash`, `grep`, `glob`). Piped stdin is appended to the prompt; `--json` streams every event as NDJSON.
- `harness models`: lists models from local servers (Ollama, LM Studio, llama.cpp) and configured providers.
- Providers: any OpenAI-compatible endpoint. Built in: `ollama`, `lmstudio`, `llamacpp`, `openrouter` (`OPENROUTER_API_KEY`).
- Approval modes: `plan`, `read-only`, `ask`, `auto`, `full-access`. Default is `auto` inside a git repository and `ask` elsewhere.
- Exit codes for scripting: `0` success, `1` runtime error, `2` invalid usage or no model, `3` an action was blocked for lack of approval, `130` interrupted.

**No OS sandbox yet.** Until phase P2 lands, every shell command needs approval, and headless runs refuse them unless you pass `--mode full-access`. Only use `full-access` in a disposable checkout.

## Quick start

Requires Rust 1.98 (pinned in `rust-toolchain.toml`) on macOS or Linux.

```sh
cargo build --release -p harness-cli
./target/release/harness models
git diff | ./target/release/harness --model ollama/qwen3:14b ask "summarize this diff"
```

Configuration lives in `~/.config/harness/config.toml` (XDG directories; `HARNESS_HOME` overrides them):

```toml
model = "ollama/qwen3:14b"

[providers.work]
protocol = "openai-chat"
base_url = "https://llm.example.com/v1"
api_key_env = "WORK_API_KEY"
```

## Roadmap

| Milestone | Scope |
|---|---|
| M1 Core agent | Phases P1 foundation (done), P2 safety (sandbox, rules, workspace trust), P3 memory (AGENTS.md, slash commands, sessions, rewind), P4 providers (ChatGPT sign-in, Anthropic, model profiles), P5 terminal UI |
| M2 Routing | Model roles, boundary-based switching, usage ledger and "$ saved", verification gates |
| M3 Agents | Subagents, delegation to Claude Code and Codex, parallel agents in worktrees |
| M4 Ecosystem | Hooks, MCP, Agent Skills, ACP server |
| M5 Distribution | Installers, native Windows |

Design and specifications: [`openspec/changes/add-core-agent/`](openspec/changes/add-core-agent/). Claude subscriptions are only ever used through the official `claude` binary; harness never reads or reuses Claude credentials.

## Contributing

Development follows the spec-driven workflow in [`AGENTS.md`](AGENTS.md): OpenSpec proposals for non-trivial changes, then plan → tests first → review.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.
