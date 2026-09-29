# harness

A terminal-first, open-source coding agent written in Rust, built for **hybrid development**: mix local models with frontier models so you get work done while saving subscription usage.

> **Status: early development.** Milestone 1, phases P1 (foundation), P2 (safety) and P3 (memory) are complete: a headless `harness ask` that runs multi-step coding tasks against any OpenAI-compatible model in a sandboxed environment, with project instructions, slash commands, saved sessions and checkpoints. More providers and the interactive terminal UI are in progress. Not ready for daily use yet.

## What works today

- `harness ask "<prompt>"`: runs one task to completion with six tools (`read`, `write`, `edit`, `bash`, `grep`, `glob`). Piped stdin is appended to the prompt; `--json` streams every event as NDJSON.
- `harness models`: lists models from local servers (Ollama, LM Studio, llama.cpp) and configured providers.
- Providers: any OpenAI-compatible endpoint. Built in: `ollama`, `lmstudio`, `llamacpp`, `openrouter` (`OPENROUTER_API_KEY`).
- Approval modes: `plan`, `read-only`, `ask`, `auto`, `full-access`. Default is `auto` inside a git repository and `ask` elsewhere.
- Exit codes for scripting: `0` success, `1` runtime error, `2` invalid usage or no model, `3` an action was blocked for lack of approval, `130` interrupted.
- Project instructions: `AGENTS.md` (or `CLAUDE.md` where a directory has no `AGENTS.md`) from `~/.config/harness/`, the repository root, and each directory down to the working directory, with `@path` import lines. They go into a system prompt that stays the same for the whole run, so model servers can reuse their prompt caches.
- Slash commands in `harness ask`: Markdown commands from `.harness/commands`, `.claude/commands` or `.opencode/commands` in the project, and from `~/.config/harness/commands` and `~/.claude/commands`, so OpenSpec's `/opsx:*` commands work as they are. `$ARGUMENTS`, `$1`…`$9`, `@file` and `` !`command` `` are filled in; a shell command gets the arguments as its parameters (`"$1"`, `"$ARGUMENTS"`), never written into its text, and shell commands go through the same approvals and sandbox as the `bash` tool. `harness ask "/init"` drafts an `AGENTS.md`.
- Sessions: every run is saved under `~/.local/share/harness/sessions/`. `harness -c ask "..."` continues the project's most recent session, `harness --resume` lists them, and `harness --resume <id> ask "..."` continues one.
- Checkpoints: before a turn first changes anything, the workspace is snapshotted into a separate git repository in harness's data directory; your own repository, index and history are never touched. Rewinding to a checkpoint arrives with the terminal UI.
- Compaction: when a conversation nears the context window, or a provider says a request is too long, older messages are replaced by a summary the model writes, and the summary is shown.

**Sandboxed by default.** In every mode except `full-access`, every shell command runs in an OS sandbox (Seatbelt on macOS, Landlock + seccomp on Linux 6.2+): no network access, and writes only inside the workspace and temp directories (none in `plan`/`read-only`). Git hooks, repository config and `.harness/` stay read-only inside the sandbox on macOS, and on Linux wherever unprivileged user namespaces work (the full tier: read-only mounts in a user namespace). Where they are blocked (stock Ubuntu 24.04 and later, most containers, the basic tier), harness checks them after each command instead, moving anything planted to a quarantine directory and restoring what changed; `harness sandbox doctor` shows how to turn on full protection. Destructive commands (force-push, `reset --hard`, `rm -rf` of the workspace), commands the analyser cannot fully parse, and re-running a command without the sandbox when the sandbox may have blocked it all need approval; headless runs refuse them and exit `3`. `plan` and `read-only` never offer that re-run. If the system has no usable sandbox, harness warns and asks before every command, and `plan`/`read-only` refuse shell commands. A workspace that is your home directory or one of its parents gets no writable sandbox, since it would cover your dotfiles: harness warns, and `ask` and `auto` ask before every command there.

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
# Linux: ask before every command when git metadata can only be checked after the fact
# linux_git_protection = "required"

[compaction]
threshold_percent = 80    # summarize at this share of the context window
keep_recent_percent = 20  # keep this share of recent messages as they are
```

Project-level `.harness/config.toml` settings that widen what the agent may do (allow rules, `read_dirs`, model, providers, sandbox settings, a `mode` wider than your global or default mode, a `max_steps` above your global limit) only apply after `harness trust`, and so does a `[compaction] threshold_percent` below 50. The same trust lets a project's command files choose their own `model`; a repository with command files and no project settings can be trusted too.

## Known limitations

- **Linux git metadata without user namespaces (the basic tier):** where unprivileged user namespaces are blocked (stock Ubuntu 24.04 and later, Docker's default profile, GitHub's Ubuntu runners), harness protects git metadata only after the fact. After each sandboxed command it moves planted hooks, config and repositories to a quarantine directory in harness's data directory (`~/.local/share/harness/quarantine/` by default) and restores changed files, so a git process running outside the sandbox at that moment, such as an editor's status poll, could read a planted file first. A change you make yourself to `.git/config` or hooks while a command runs is undone too and kept in the quarantine. Only a kernel or host refusal drops a session to this tier; a one-off race only blocks that one command. `harness sandbox doctor` shows the one-time fix (an AppArmor profile for the harness binary, or a sysctl); `sandbox.linux_git_protection = "required"` makes harness ask before every command instead.
- **Linux git metadata in either tier:** mounts and checks cannot cover a name that does not exist yet. A new `commondir` in a gitdir, a top-level `HEAD` or `.harness/` is moved to the quarantine milliseconds after it appears rather than refused, a new nested repository only after the command ends, and one inside a git-ignored directory not at all. A repository or gitdir you add between commands can also be changed by a process an earlier command left running, before the next command's guard snapshots it; it's left alone as your own, since a repository created between commands is taken for your own clone. While a command runs, the watcher that catches a change undoes it but does not stop the command that made it.
- **The full tier also restores.** Renaming a file from outside its mount namespace, as git does whenever it rewrites `.git/config`, detaches the read-only bind on it in every other namespace, so a command running while the user runs git could otherwise write the new file. So the full tier snapshots and restores protected files the same way the basic tier does; a user's own `.git/config` edit made while a command runs is undone and kept in the quarantine too.
- **The quarantine** lives at `$XDG_DATA_HOME/harness/quarantine/<timestamp>/` and nothing there is ever deleted. A quarantined repository has every `.git` renamed to `dot-git` and every `HEAD` to `HEAD.quarantined`, so git refuses to treat it as one; rename it back in a copy to inspect it. A second version of the same path quarantined within one command is stored as `<name>.<n>`. If the quarantine itself can't be used, an entry is renamed in place as `<name>.harness-quarantine-<n>` instead; an entry harness can't move at all is reported, retried before each later command, and forgotten on restart.
- **Background processes** a sandboxed command leaves running are tracked in both Linux tiers; harness registers itself as a child subreaper so they reparent to it rather than becoming orphans elsewhere. While any one of them is alive, changes to protected files keep getting undone before each later command, your own included. A daemon a command starts (`gpg-agent`, `git credential-cache`, git's detached auto-gc) keeps this on until it exits. When harness exits, it ends the processes sandboxed commands left running (`SIGTERM`, then `SIGKILL` about a second later), checks git metadata once more, undoing what they changed (if there were none, changes since the last command are yours and are left alone), and says what it did on stderr; the exit code does not change, and a daemon a command started is ended with the rest. Processes in harness's own session, such as a command you approved to run outside the sandbox, are left alone. An orphan that stays in harness's own session, rather than one it reparents, is left as a zombie until harness exits.
- **Ignore rules** (`.gitignore` files and each repository's `info/exclude`) are read once, when the session starts, so a command can't hide a new repository behind a rule it writes mid-session; a rule you add yourself takes effect after a restart. A repository created in, or moved into, an already-ignored directory is not caught on Linux (macOS blocks it). Ignore rules written by a command in an earlier session count as the user's.
- **Scanning and change limits.** A workspace scan stops after 200,000 entries or 5 seconds and says so once per session; a command that makes a fully scannable workspace unscannable counts as blocked. When a check on Linux hits its 10,000-change cap, the next command also undoes changes made between commands, the user's own included. The Linux guard's snapshot of protected files records at most 20,000 entries, 16 MiB and 16 levels below a protected entry; past that, a file added deep inside a protected directory is not seen in the basic tier, which only matters with well over a thousand gitdirs.
- **Linux kernels older than 6.2** (Landlock ABI < 3) get no sandbox, so harness asks before every command.
- **Linux `/dev/shm`** is writable from the sandbox in `ask` and `auto` modes, because Python's `multiprocessing` needs it.
- **No namespaces inside the Linux sandbox:** sandboxed commands cannot create user, mount or other namespaces (seccomp refuses `unshare` outright and `clone` with any `CLONE_NEW*` flag, and fails `clone3` with `ENOSYS`), so programs that sandbox themselves that way (Chromium and Electron, bubblewrap, rootless Podman) need their no-sandbox mode or an unsandboxed re-run.
- **Shell analysis is best effort.** Deny rules match the commands harness can see through wrappers such as `env`, `sudo`, `bash -c` and `xargs`. They do not see inside script files or interpreters (`python -c`, `node -e`), and wrappers such as `busybox` are not unwrapped. A command substitution that bash could end elsewhere than the analyser needs approval: macOS `/bin/bash` 3.2 reads `$(…)` its own way, and bash sees comments there that the parser can miss. So does a `shopt` that changes an option (or `bash -O`), since options such as `extglob` and `expand_aliases` change how bash reads later commands. Commands longer than 256 KiB are not checked against deny rules at all, so they need approval (in `full-access`, whenever a deny rule is set). The OS sandbox is the security boundary; rules are guardrails.
- **`git -c` asks.** Git configuration on the command line can run programs, so every `git -c` needs approval unless its key is one that cannot: `user.name`, `user.email`, `init.defaultBranch`, `color.*`, `advice.*`, `core.quotepath`, and `commit.gpgsign`/`tag.gpgsign` set to `false`.
- **Sandboxed commands can read any file.** The sandbox limits writes and network access, not reads. `read:` and `write:` rules govern only the file tools, not what a shell command opens, and a `read:` deny rule on a single file does not stop `grep` or `glob` from searching the directory that holds it.
- **Hard links:** on macOS, files with more than one hard link cannot be modified inside the sandbox. On Linux, hard links that already point outside the workspace stay writable.
- **Git inside the sandbox** cannot create repositories or worktrees in the workspace (`git init`, `git clone`, `git worktree add`); on Linux they are created and then moved to the quarantine. On macOS a nested repository can still be moved out of the workspace, edited and moved back. `.git/rebase-merge/git-rebase-todo` stays writable, so a sandboxed command could add `exec` lines that run the next time you continue a rebase (`git rebase --continue`). A workspace that is a linked worktree (its `.git` file points into another repository's `.git/worktrees/`) cannot commit inside the sandbox, because that gitdir is outside the workspace. A repository outside the workspace in a writable temp or `writable_roots` directory gets no protection.
- **Path rules** are matched after resolving symlinks. On macOS, write `/private/tmp/...` rather than `/tmp/...` in `allow` rules.
- **One context window for every model.** Until model profiles arrive, harness assumes 32,768 tokens: compaction starts at 80% of that, and instruction files over a quarter of it get a warning. A server with a smaller window that rejects a long request gets one compacted retry; one that silently truncates (Ollama's default) does not.
- **Checkpoints** cover the working directory, not files over 10 MB, git-ignored files, `node_modules`, `target`, `.harness/` or a top-level `HEAD`, or what is inside nested repositories; a rewind leaves those alone, including a file that was ignored or too large when the checkpoint was taken. In a subdirectory of a repository, the repository's own ignore rules apply, unless they ignore that subdirectory itself (then only its own `.gitignore` files do); your global git excludes file does not. A checkpoint is restored only in the directory it was taken in, so a session continued from another directory can rewind its conversation but not those files. git stores only whether a file is executable: a restored file gets your umask's permissions, except files only you could read, which get theirs back. Checkpoints are off, with a warning, when harness's data directory is inside the workspace or a directory sandboxed commands can write to (a temp directory, or a `writable_roots` entry), and in a workspace whose first snapshot takes longer than 5 seconds. They need git 2.26 or later.
- **Command files run with their own settings.** A command file's `allowed-tools` pre-approve the commands it names (never beyond deny rules, destructive-command confirmation or the sandbox). Its `model` answers its invocations if the file is your own (`~/.config/harness/commands`, `~/.claude/commands`); a project's command file chooses the model only once `harness trust` has trusted the directory the command files come from, the repository root (run it there, not in a subdirectory); otherwise a note says the session's model answers instead. In a command file's `` !`…` `` commands, the arguments are shell parameters set before the command runs: write `"$1"` or `"$ARGUMENTS"` in double quotes, as in any script (an unquoted `$1` is split and globbed, and `'$1'` is the text `$1`). A command that uses the arguments with a construct where bash may evaluate a value as code or arithmetic (`$((…))`, `$[…]`, `let`, `declare`, subscripts, `=(…)`, `eval`, `trap`, `read`, `printf -v`, `source`, `.`, `${!…}`, `${…@P}`, `compgen`, `complete`, `enable`, and a few more) is not run, with a warning. That list defends ordinary command bodies; one that deliberately hands an argument to something that runs it later, such as `PS4`, `PROMPT_COMMAND` or `BASH_ENV`, is the command author's responsibility, as in any script. Read command files from repositories you did not write before running them.
- **Instruction files and command files are read when a run starts**; changes apply to the next run.

## Roadmap

| Milestone | Scope |
|---|---|
| M1 Core agent | Phases P1 foundation, P2 safety and P3 memory (AGENTS.md, slash commands, sessions, checkpoints, compaction) done; P4 providers (ChatGPT sign-in, Anthropic, model profiles), P5 terminal UI (rewind picker, `/compact`, `/resume`) |
| M2 Routing | Model roles, boundary-based switching, usage ledger and "$ saved", verification gates |
| M3 Agents | Subagents, delegation to Claude Code and Codex, parallel agents in worktrees |
| M4 Ecosystem | Hooks, MCP, Agent Skills, ACP server |
| M5 Distribution | Installers, native Windows |

Design and specifications: [`openspec/changes/add-core-agent/`](openspec/changes/add-core-agent/). Claude subscriptions are only ever used through the official `claude` binary; harness never reads or reuses Claude credentials.

## Contributing

Development follows the spec-driven workflow in [`AGENTS.md`](AGENTS.md): OpenSpec proposals for non-trivial changes, then plan → tests first → review.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.
