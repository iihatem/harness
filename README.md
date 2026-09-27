# harness

A terminal-first, open-source coding agent written in Rust, built for **hybrid development**: mix local models with frontier models so you get work done while saving subscription usage.

> **Status: early development.** Milestone 1, phases P1 (foundation) and P2 (safety) are complete: a headless `harness ask` that runs multi-step coding tasks against any OpenAI-compatible model in a sandboxed environment. Sessions, more providers, and the interactive terminal UI are in progress. Not ready for daily use yet.

## What works today

- `harness ask "<prompt>"`: runs one task to completion with six tools (`read`, `write`, `edit`, `bash`, `grep`, `glob`). Piped stdin is appended to the prompt; `--json` streams every event as NDJSON.
- `harness models`: lists models from local servers (Ollama, LM Studio, llama.cpp) and configured providers.
- Providers: any OpenAI-compatible endpoint. Built in: `ollama`, `lmstudio`, `llamacpp`, `openrouter` (`OPENROUTER_API_KEY`).
- Approval modes: `plan`, `read-only`, `ask`, `auto`, `full-access`. Default is `auto` inside a git repository and `ask` elsewhere.
- Exit codes for scripting: `0` success, `1` runtime error, `2` invalid usage or no model, `3` an action was blocked for lack of approval, `130` interrupted.

**Sandboxed by default.** In every mode except `full-access`, every shell command runs in an OS sandbox (Seatbelt on macOS, Landlock + seccomp on Linux 6.2+): no network access, and writes only inside the workspace and temp directories (none in `plan`/`read-only`). On macOS, git hooks and `.git` configuration stay read-only inside the sandbox; on Linux they do not yet (see Known limitations). Destructive commands (force-push, `reset --hard`, `rm -rf` of the workspace), commands the analyser cannot fully parse, and re-running a command without the sandbox when the sandbox may have blocked it all need approval; headless runs refuse them and exit `3`. `plan` and `read-only` never offer that re-run. If the system has no usable sandbox, harness warns and asks before every command, and `plan`/`read-only` refuse shell commands. A workspace that is your home directory or one of its parents gets no writable sandbox, since it would cover your dotfiles: harness warns, and `ask` and `auto` ask before every command there.

Set `HARNESS_SANDBOX=none` to turn the sandbox off (every shell command then needs approval, and `plan`/`read-only` refuse them).

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

[permissions]
allow = ["bash:cargo test*", "write:docs/*"]
deny = ["bash:git push*"]
confirm = ["bash:terraform apply*"]

[sandbox]
writable_roots = ["~/.cargo"]
```

Project-level `.harness/config.toml` settings that widen what the agent may do (allow rules, `read_dirs`, model, providers, sandbox settings, a `mode` wider than your global or default mode, a `max_steps` above your global limit) only apply after `harness trust`.

## Known limitations

- **Linux git metadata:** Landlock can only grant access, so inside a writable workspace it cannot keep `.git/hooks` and `.git/config` read-only. A sandboxed command could plant a git hook that runs on your next `git` command. A stronger Linux backend is planned.
- **Linux kernels older than 6.2** (Landlock ABI < 3) get no sandbox, so harness asks before every command.
- **Linux `/dev/shm`** is writable from the sandbox in `ask` and `auto` modes, because Python's `multiprocessing` needs it.
- **Shell analysis is best effort.** Deny rules match the commands harness can see through wrappers such as `env`, `sudo`, `bash -c` and `xargs`. They do not see inside script files or interpreters (`python -c`, `node -e`), and wrappers such as `busybox` are not unwrapped. On macOS, `/bin/bash` 3.2 reads a here-document inside `$(…)` differently from the analyser, so specially crafted text there can hide a command from deny rules; the command still runs inside the sandbox. The OS sandbox is the security boundary; rules are guardrails.
- **`git -c` asks.** Git configuration on the command line can run programs, so every `git -c` needs approval unless its key is one that cannot: `user.name`, `user.email`, `init.defaultBranch`, `color.*`, `advice.*`, `core.quotepath`, and `commit.gpgsign`/`tag.gpgsign` set to `false`.
- **Sandboxed commands can read any file.** The sandbox limits writes and network access, not reads. `read:` and `write:` rules govern only the file tools, not what a shell command opens, and a `read:` deny rule on a single file does not stop `grep` or `glob` from searching the directory that holds it.
- **Hard links:** on macOS, files with more than one hard link cannot be modified inside the sandbox. On Linux, hard links that already point outside the workspace stay writable.
- **Git inside the sandbox** cannot create repositories or worktrees in the workspace (`git init`, `git clone`, `git worktree add`). A nested repository can still be moved out of the workspace, edited and moved back. `.git/rebase-merge/git-rebase-todo` stays writable, so a sandboxed command could add `exec` lines that run the next time you continue a rebase (`git rebase --continue`). A workspace that is a linked worktree (its `.git` file points into another repository's `.git/worktrees/`) cannot commit inside the sandbox, because that gitdir is outside the workspace. A repository outside the workspace in a writable temp or `writable_roots` directory gets no protection.
- **Path rules** are matched after resolving symlinks. On macOS, write `/private/tmp/...` rather than `/tmp/...` in `allow` rules.

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
