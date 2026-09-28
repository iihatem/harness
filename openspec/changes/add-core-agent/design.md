# Design: add-core-agent (Milestone 1)

## Context

Greenfield Rust project (see proposal.md for motivation). Decisions below were made during a brainstorming session on 2026-09-24, informed by two research rounds: vendor auth policy and existing harness architectures, then a survey of 30+ coding agents, developer surveys, and GitHub feature-request data (References at the end).

Constraints that shape everything:

- **Audience:** public open source, so vendor terms and safety defaults matter more than convenience.
- **Vendor policy (as of 2026-09):** Anthropic forbids third-party apps from using Claude Free/Pro/Max credentials and blocks it server-side; only the unmodified `claude` binary may use them. OpenAI publicly tolerates ChatGPT sign-in in third-party harnesses (pi, OpenCode, Amp) and exposes `codex app-server`. Gemini CLI / Antigravity OAuth reuse is banned; Qwen's free OAuth tier is discontinued.
- **Platforms:** macOS and Linux in v1; Windows via WSL until M5.
- **Reference implementations:** vercel-labs/fx (Zig; Unix-like, minimal, embeddable); openai/codex (Rust, Apache-2.0) for sandboxing, the inline TUI, and ChatGPT auth; pi for provider-neutral branching sessions and a sub-1k-token system prompt; Claude Code for plan mode, `/rewind`, and command formats; Capy and Aider for the planner/builder split that M2 builds on.

### What the research changed

| Finding | Design response |
|---|---|
| Plan mode and rewind are table stakes (Codex #2101 406👍, #9203 460👍) | D12 plan mode, D13 checkpoints in M1 |
| Users approve 93–97% of permission prompts; prefix rules are bypassed with `a && b` | D5 sandbox-first autonomy, compound-command parsing, destructive-command list |
| Agents deleted production data using credentials found in unrelated files | D5 reads outside the workspace need approval |
| Local users suffer silent context truncation, broken tool formats, cache-busting prompts | D2 model profiles, D14 local-model robustness, D15 cache-stable prefix |
| Opaque quotas and silent model switches drive churn; automatic routers were removed by Goose three times | Every message attributed to its model; M1 never switches models on its own; M2 routes only at boundaries |
| XDG compliance (CC #1455 431👍) and multiple accounts (CC #18435 829👍) are top requests | D9 XDG paths; D3 account profiles |

## Goals / Non-Goals

**Goals:**

- A lean, embeddable core whose frontends are thin consumers of one event stream.
- First-class local models: short system prompt, stable prompt prefix, tight tool output, robust tool-call handling, per-model profiles.
- Safe by default for a large user base: OS sandbox, approval modes, checkpoints, no silent unsandboxed fallback.
- Transparency: the user always knows which model answered, how much context is used, and what the harness sends where. No telemetry.
- Drop-in compatibility with existing `AGENTS.md`, `CLAUDE.md`, and Markdown command setups.
- Interfaces that leave room for M2–M5 without implementing them.

**Non-Goals (M1):**

- Routing, fallback chains, usage ledger, verification gates, LSP (M2); subagents, delegation to Claude Code or Codex, worktree-parallel and background agents (M3); hooks, MCP, Skills, ACP, custom statusline, image input (M4); installers, native Windows, remote steering, scheduling (M5).
- Web fetch, a session-tree browser beyond `/rewind`, a plugin/extension API, Gemini TOML commands, hashline or diff edit formats (M2 decides edit formats with its eval suite).

## Decisions

### D1. Library core with thin frontends (single binary)

Cargo workspace:

```
crates/
  harness-core       agent loop, session model, event stream, Tool trait, permission engine, checkpoints
  harness-providers  Provider trait; openai-chat, openai-responses, anthropic-messages adapters; model profiles; credential store
  harness-tools      read, write, edit, bash, grep, glob
  harness-sandbox    macOS Seatbelt; Linux Landlock + seccomp
  harness-context    AGENTS.md/CLAUDE.md discovery; prompt assembly; slash-command discovery and expansion
  harness-config     XDG paths, config layering, workspace trust
  harness-cli        `harness` binary: clap subcommands, inline TUI, `ask`, NDJSON output
```

The core runs on `tokio` and emits `AgentEvent`s: `TurnStarted`, `TextDelta`, `ReasoningDelta`, `ToolCallRequested`, `ApprovalNeeded`, `ToolCallFinished`, `Usage`, `TurnStats`, `Retrying`, `Compacted`, `CheckpointCreated`, `TurnFinished { reason }`, `Error { kind }`. Every assistant message carries the id of the model that produced it. Frontends send back `UserInput { delivery: Queued | SendNow }`, `ApprovalDecision`, and `Interrupt`. The TUI, `ask` (plain), and `ask --json` (NDJSON) are consumers; the M4 ACP server will be another.

*Alternatives:* a local daemon plus clients (OpenCode-style) enables shared live sessions but adds a daemon, ports, and auth, and OpenCode draws criticism for 1 GB+ memory use; a routing proxy (claude-code-router-style) cannot route Claude subscriptions compliantly. Rejected for M1; the event-stream core makes a daemon an additive frontend later.

### D2. Provider-neutral messages, three wire adapters, model profiles

Conversation history is stored in an internal, provider-neutral format. Each adapter translates to and from its wire protocol:

| Adapter | Protocol | Covers |
|---|---|---|
| `openai-chat` | `/v1/chat/completions` (SSE) | Ollama, LM Studio, llama-server, vLLM-MLX, OpenRouter, Groq, Gemini's OpenAI-compatible endpoint |
| `openai-responses` | `/v1/responses` (SSE) | OpenAI API keys; ChatGPT sign-in |
| `anthropic-messages` | `/v1/messages` (SSE) | Anthropic API keys; Anthropic-compatible local servers |

Model ids are `<provider>/<model>` (e.g. `ollama/qwen3-coder:30b`, `chatgpt/<model>`). On first interactive use with no configured model, the model picker opens and saves the choice as the global default; `harness ask` never picks a model implicitly. When switching models, provider-specific content the target cannot accept (e.g. signed reasoning blocks) is dropped.

**Model profiles** are TOML tables keyed by model-id glob (`[profiles."ollama/qwen3-coder*"]`) with fields `context_window`, `min_context`, `max_output_tokens`, `temperature`, `reasoning_effort`, `text_tool_calls` (on/off), and `local` (bool). Resolution order: user config → built-in profiles shipped in the binary for common open-weight coding families → protocol defaults. `Provider` is a trait so M2's router can be a provider that delegates to other providers; delegated agents (M3) will use a separate `AgentBackend` trait. M1 defines neither.

### D3. Credentials and account profiles

- Resolution per provider and account profile: configured env var (e.g. `OPENAI_API_KEY`) → stored credential for the active profile.
- Storage: OS keychain via `keyring`, keyed by provider and profile; fallback `$XDG_DATA_HOME/harness/credentials.json` with mode `0600` and a warning when no keychain service exists. Credentials never live in the config directory, which users often sync to dotfile repositories.
- Profiles: `harness login chatgpt --profile work`, `harness auth use <provider> <profile>` selects the default; the unnamed profile is `default`.
- ChatGPT sign-in: OAuth PKCE in the browser with a localhost callback; device-code flow with `--device` or when no browser can be opened. Tokens refresh automatically. Isolated behind a default-on Cargo feature so it can be disabled quickly if OpenAI's policy changes.
- Claude subscription credentials are never read, stored, or used.

### D4. Tools

Six tools with short descriptions and a fixed definition order. `write`/`edit` enforce read-before-overwrite with a content hash. `bash` runs `bash --noprofile --norc -c` with `BASH_ENV` and `ENV` removed (bash is looked for only at `/bin/bash`, `/usr/bin/bash` and `/run/current-system/sw/bin/bash`, never on `PATH`; `/bin/sh -c` is the fallback only when none of them exists), so the shell that runs a command is the shell whose syntax the rules engine analysed (D16). Outside `full-access` the command is wrapped by the OS sandbox (D5); the child becomes the leader of its own process group so timeouts and interrupts kill everything it started. Tool output larger than 10 KB, or larger than the active model's output budget, is saved to `$XDG_STATE_HOME/harness/tool-output/<session>/<call-id>.txt`; the model receives the head, the tail, the omitted size, and the file path, which it may `read` without approval. `grep`/`glob` use ripgrep's `ignore` and `grep` crates.

### D5. Permissions and sandbox

| Mode | File writes | `bash` | Reads outside workspace |
|---|---|---|---|
| `plan` | rejected | read-only sandbox, no network; refused when no sandbox is available | ask |
| `read-only` | rejected | read-only sandbox, no network; refused when no sandbox is available | ask |
| `ask` | every write asks unless allow-listed | every command asks unless allow-listed; sandboxed | ask |
| `auto` | in-workspace allowed | sandboxed (workspace + temp writable, no network) without asking | ask |
| `full-access` | allowed | unsandboxed, no prompts (explicit flag only, persistent warning) | allowed |

- Default mode: `auto` inside a git work tree, `ask` elsewhere. Shift+Tab cycles `plan → ask → auto` (never into `full-access`).
- Rules (`allow`/`deny`, `<tool>:<glob>`) are evaluated before prompting; deny always wins, in every mode.
- **Compound commands:** shell commands are parsed (sequences, `&&`, `||`, pipes, subshells, command substitution) into sub-commands. A command is allowed without prompting only if every sub-command is allowed; it is blocked if any sub-command is denied. Unparseable commands require approval.
- **Destructive commands** always require approval outside `full-access`, even in `auto` or when allow-listed: `git push --force`/`-f`, `git reset --hard`, `git clean` with `-f`, `git checkout -- .`/`git restore .`, `rm -r` targeting the workspace root or a path outside it, and any user-configured `confirm` patterns.
- Reads (`read`, `grep`, `glob`) outside the workspace require approval except for harness's own tool-output directory and configured `read_dirs`.
- When a sandboxed command fails with what looks like a sandbox denial, the interactive UI offers to re-run it unsandboxed with approval, except in `plan` and `read-only`, which report the denial and never run a command with write access. If no sandbox mechanism is available, every `bash` call requires approval in `ask` and `auto` and is refused in `plan` and `read-only` ("shell commands need the OS sandbox in plan and read-only mode"; a matching deny rule's reason takes precedence). There is never a silent unsandboxed fallback.
- **Too-broad workspaces:** a workspace that is `/`, `$HOME` or an ancestor of `$HOME` gets no workspace-write sandbox, since it would make the user's dotfiles writable. `ask` and `auto` then run as if no sandbox were available, after a one-line warning; `plan` and `read-only` keep their read-only sandbox. The check is the one the sandboxes apply to temp and cache roots.
- **macOS** uses `/usr/bin/sandbox-exec` (absolute path) with a generated Seatbelt profile, as Codex and Claude Code do. **Linux** uses Landlock for the filesystem and seccomp for the network, applied in the forked child before exec (no helper binary). seccomp refuses creating any socket other than `AF_UNIX`, and refuses `connect`, `bind`, `listen` and `accept` for every family, `AF_UNIX` included, so Unix-domain sockets work only as `socketpair`s; it also refuses io_uring and, on x86_64, the x32 ABI. Landlock ABI ≥ 3 (kernel 6.2+) is required, because earlier ABIs cannot restrict truncation. On Linux the network denial also covers localhost; `sandbox.allow_localhost` works on macOS only.
- **Linux git-metadata protection** comes in two tiers, because Landlock can only grant access and cannot make a path read-only inside a writable workspace.
  - **Choosing the tier.** At startup harness probes a throwaway child: a user and mount namespace, a 1:1 uid/gid map, and one test bind mount. If the probe succeeds the session runs in the **full** tier; otherwise it runs in the **basic** tier. Stock Ubuntu 24.04 and later, Docker's default seccomp profile and GitHub's Ubuntu runners block unprivileged user namespaces; Debian, Fedora, Arch, openSUSE and NixOS allow them.
  - **Full tier.** In the same forked child, before `no_new_privs`, Landlock and seccomp, the child:
    - unshares a user and mount namespace, maps the real uid and gid 1:1, and makes mount propagation private;
    - pins each gitdir (the top-level `.git`, following a symlink or gitfile, then `.git/modules/**`, `.git/worktrees/*`, and nested repositories from the guard's index) with a read-write self-bind, so the entry cannot be renamed, removed or replaced; the directories on the way to a gitdir (`.git/modules`, `.git/worktrees`, and the folder that holds a separate git directory) are pinned too, because Linux lets a command rename a parent of a mount point, which would let a new gitdir take the path a gitfile names;
    - self-binds read-only every protected entry that exists: `config`, `config.worktree`, `commondir`, `hooks/`, `gitweb/`, `pid` and the matching `worktrees/<id>` entries in each gitdir, plus every gitfile `.git`, `.harness/` and a top-level `HEAD`. The parent creates an empty `hooks/` placeholder when it is missing, which git treats exactly like a missing directory;
    - changes directory again so its working directory resolves inside the new mounts.
    
    In both tiers the seccomp filter also refuses the mount and namespace syscalls (`mount`, `umount2`, `mount_setattr`, `move_mount`, `open_tree`, `open_tree_attr`, `fsopen`, `fsconfig`, `fsmount`, `fspick`, `pivot_root`, `unshare`, `setns`, and `clone` with any `CLONE_NEW*` flag), and makes `clone3`, whose flags it cannot read, fail with `ENOSYS`, so runtimes fall back to `clone`. Mounts copied into any nested namespace are locked. A workspace with nothing to protect skips the namespace. If any step fails the command does not run. When the kernel or host refuses a step (the namespace, the id maps, or a mount call itself), the session drops to the basic tier with a warning; when a step fails because a path changed in the meantime, or a resource limit was hit, only that command is blocked and the session keeps the full tier, so something outside a command cannot downgrade the session.
  - **Basic tier.** Landlock, seccomp and the guard, with one startup warning naming `harness sandbox doctor`. That command reports the tier, why it was chosen, and the exact fix (an AppArmor profile for the harness binary, or the sysctl); it never changes system files itself. Setting `sandbox.linux_git_protection = "required"` makes the basic tier ask before every shell command in `ask` and `auto` (`plan` and `read-only` keep their read-only sandbox, which already protects git metadata); the default, `"best-effort"`, keeps `auto` usable.
  - **The guard, in both tiers.** Mounts cannot cover a name that does not exist yet, and there is no safe placeholder for `commondir` (git refuses an empty one). The guard runs in the harness process around each sandboxed command. While the command runs, an inotify watcher on each known gitdir and on the workspace root catches a new `commondir`, `config`, `config.worktree`, `hooks`, `gitweb` or `pid` in a gitdir, a top-level `HEAD` and `.harness/`. These are the places where a git process outside the sandbox, such as an editor's background status poll, would pick up a planted file at once. In both tiers the watcher also catches changes to the existing protected files and gitfiles, which the guard snapshots before the command: in the full tier too, because renaming or removing a file from outside a mount namespace, as git does whenever it rewrites `.git/config`, detaches the read-only bind on it in every other namespace, so a command running while the user runs git could otherwise write the new file; in both tiers the guard snapshots what a mount cannot protect: a protected symlink, and a protected file with a second hard link. After the command, a final check repeats the gitdir checks, and a scan of the workspace (skipping git-ignored directories) then finds any new nested `.git`. Before the next command the guard checks again for new protected names in the gitdirs it knew and at the top of the workspace, to catch anything a background process left behind; a repository created between commands, which may be the user's own clone, is left alone.
    - The scan reads the workspace's ignore rules (`.gitignore` files and each repository's `info/exclude`) once, when the session starts, and uses them for every scan. So a command cannot hide a new repository behind an ignore rule it wrote, and neither can the model's file tools. The cost: a rule you add mid-session takes effect after a restart. Ignore files are read only if they are regular files, within size and complexity limits. Each scan stops after a fixed number of entries or 5 seconds. A scan that could not cover the whole workspace is reported once per session. If a command makes a fully scanned workspace impossible to scan fully, that command counts as blocked. A `.git` or nested gitdir absent before the command counts as new when the scan before the command was complete, since that scan saw everything; otherwise it is reported and left in place.
    - **Background processes.** A command can leave a process running that changes a protected file after the command ends. In the full tier such a process stays in the command's mount namespace, so the entries mounted read-only there stay protected, but a protected entry that appears later (a worktree or submodule gitdir added afterwards) is not mounted in that namespace. So in both tiers harness tracks the processes each command leaves running, including detached ones: it registers as a child subreaper, so orphans reparent to it. While any of them lives, the watcher keeps checking after the command ends, and the check before the next command also restores changed protected files and gitfiles from the snapshot. Restoring also undoes a change you make to those files while such a process runs. The changed version is kept in the quarantine and reported. When no such process is left, changes between commands are yours and are left alone. When harness exits, it ends the processes sandboxed commands left running (`SIGTERM`, then `SIGKILL` after about a second), runs one last check that undoes any change they made to protected files, and prints what it did to stderr without changing the exit code; processes from commands you approved to run outside the sandbox are left alone.
    - Anything found is moved, never deleted, to `$XDG_DATA_HOME/harness/quarantine/<timestamp>/`. Across filesystems it is copied and the original removed; if copying fails it is renamed in place (`<name>.harness-quarantine-<n>`) so git ignores it. In the quarantine every `.git` path component is stored as `dot-git`, so git never treats a quarantined repository as one or runs its config. Every move, copy, removal and restore works through directory handles that never follow a symlink in any part of the path. A directory swapped for a symlink therefore cannot redirect them outside the workspace. Changed files are restored from the snapshot, which also undoes a change someone else makes to them while the command runs.
    - The tool result reports what the guard did, so both the model and the user see it; the command counts as blocked, so headless runs exit 3 and no re-run outside the sandbox is offered. What the check before a command finds is reported without blocking that command.
    - As on macOS, `git init` and `git clone` into the workspace are undone.
    - Known windows: between a planted gitdir file's creation and its quarantine there are milliseconds in which a git process outside the sandbox could read it. A new nested `.git` is removed only after the command ends. A `.git` created in, or moved into, a directory the session's ignore rules ignore is not detected, and ignore rules a command wrote in an earlier session count as the user's.
  - **Later provider.** A bubblewrap provider that renders the same mount plan as `bwrap` arguments may be added for systems where only the distribution's `/usr/bin/bwrap` is granted user namespaces.
- **Workspace-write sandboxes** allow writes under the workspace, `$TMPDIR`, `/tmp`, `/var/tmp`, the macOS user cache directory, `sandbox.writable_roots`, and on Linux `/dev/shm` (Python's `multiprocessing` needs it); they deny writes to `.git/config`, `.git/hooks/`, the `.git` entry itself, a top-level `HEAD`, and `.harness/`, so hooks and repository config cannot be planted while `git commit`, `checkout` and `stash` still work. On macOS the Seatbelt profile applies this to every gitdir in the workspace by pattern (any `.git`, `.git/modules/*`, `.git/worktrees/<id>`), and also protects `commondir`, `config.worktree`, `gitweb/` and `pid` in each. It additionally resolves where the top-level `<workspace>/.git` leads when that entry itself — not a nested one, such as a submodule's `sub/.git` — is a symlink or a gitfile: the gitdir it names, plus that gitdir's `commondir` target, with every entry on the way to either protected too. On Linux, inherited file descriptors above 2 are marked close-on-exec in the child before exec (`close_range`, else `/proc/self/fd`); on macOS harness does not close them and relies on descriptors being opened close-on-exec, as Rust's standard library opens them.
- **Classification in `auto`:** the rules engine (D16) returns allow-listed, unlisted, must-ask, or denied. `auto`, `plan` and `read-only` run allow-listed and unlisted commands sandboxed without a prompt; `ask` prompts for unlisted commands; must-ask always prompts outside `full-access`.
- **Sandbox denials** are recognised heuristically: a non-zero exit (other than `sandbox-exec`'s own 65 and 71) plus "operation not permitted", "permission denied", "read-only file system" or "failed to write file"; with all networking off, also "could not resolve host", "nodename nor servname", "temporary failure in name resolution", "network is unreachable" or "failed to connect" (on macOS with `allow_localhost`, networking is not all off, so these are skipped); and on Linux "invalid cross-device link" (Landlock's `EXDEV` for renames and links across a rule boundary). This list extends the original design's, except that the bare word "sandbox" is not a signal: it appears in ordinary output such as crate and file names. Because the match is a guess, the bash tool notes "the sandbox may have blocked part of this command" and the agent asks whether "the sandbox may have blocked this command" should run again outside the sandbox; headless runs report the command as blocked.
- **Approve for session** remembers per-sub-command prefixes (the program, plus the first argument for subcommand-style tools such as `git`, `cargo`, `npm`), never for destructive commands.

### D6. Context and commands

Instruction discovery walks from the working directory up to the discovery root (repository root; outside a repository, `$HOME` when inside it, otherwise the working directory), plus global `$XDG_CONFIG_HOME/harness/AGENTS.md`. Per directory, `AGENTS.md` is preferred and `CLAUDE.md` is the fallback. A project instruction file that resolves, through symlinks, outside the discovery root and the config directory is skipped with a warning; the global file may link anywhere. An import is a line holding only `@path`, outside code fences. Imports resolve relative to the importing file, up to depth 5, confined to the discovery root and the harness config directory; outside a repository, where the root can be the home directory, a file's imports stay in that file's own directory and the config directory, so a downloaded folder's `AGENTS.md` cannot import `~/.ssh/id_rsa`. Each file is included at most once; a missing or disallowed import stays as written, with a warning.

Command discovery order (first match wins): project `.harness/commands`, `.claude/commands`, `.opencode/commands` (in the repository root, or the working directory outside a repository), then global `$XDG_CONFIG_HOME/harness/commands`, `~/.claude/commands`. Subdirectories become `:` namespaces (`opsx/propose.md` → `/opsx:propose`). A project command file, or a project commands directory, that resolves outside the project is skipped with a warning; global command files may link anywhere, and a project commands directory that is also a global one (in the home directory) counts as global. Supported frontmatter: `description`, `argument-hint`, `model`, `allowed-tools` (Claude Code tool names mapped to harness rules for that invocation only; cannot bypass deny rules, destructive-command confirmation, or the sandbox). A project command file's `model` applies only in a trusted workspace (one whose project settings `harness trust` approved and still match); elsewhere it is ignored with a note, and the session's model answers. A global command file's `model` always applies. Command files are not part of the trust fingerprint. `Bash` patterns become `bash:` allow rules (`Bash(git add:*)` is `bash:git add` and `bash:git add *`); `Write`, `Edit` and `MultiEdit` become `write:` rules; `Read`, `Grep` and `Glob` add nothing, because reads inside the workspace need no rule and a command file must not widen reads outside it.

Expansion: `$ARGUMENTS` and `$1`–`$9` are filled in from the arguments, and when the body uses none of them the arguments are appended as `ARGUMENTS: <args>`, as Claude Code does. `@path` and `` !`cmd` `` are expanded only in the command body, never in the arguments. A placeholder in a `!` command is filled in quoted for where it lands, so the command gets the argument as data: outside quotes as one single-quoted word, inside `'…'` with each `'` written as `'\''`, and inside `"…"` with `\`, `"`, `$` and backticks escaped. Where that cannot be done (a placeholder after `\` or `$`, in a comment, inside `$'…'`, `${…}`, `[[ … ]]` or a subscript, or after `((`, `$((` or a `$(` within double quotes), the command never runs: it stays in the message as `` [not expanded: `cmd`] ``, with a warning. `@path` includes a workspace file that the read rules allow without asking. A `!` command runs as a `bash` tool call at the start of the turn, so it goes through the same permission check, approval, sandbox and guard; its output takes its place, or, when it does not run or fails, a note and the tool's result do, and the prompt is still sent. In `harness ask`, custom commands and `/init` run; other built-ins and unknown `/names` exit 2, while input whose first word is a path, such as `/usr/bin/env is missing`, is ordinary text.

`/init` asks the model to write `AGENTS.md` in the working directory with the `write` tool. During `/init` shell commands run read-only, and when `AGENTS.md` exists a confirm rule for that turn makes replacing it ask, so a headless run shows the proposed content and exits 3.

### D7. Sessions

Append-only JSONL at `$XDG_DATA_HOME/harness/sessions/<project-key>/<session-id>.jsonl`, where `<project-key>` is derived from the canonical repo root path (or the working directory outside a repo): its last component and 16 hex digits of the path's SHA-256. Each entry has `id` and `parent_id`; the session's active branch is the path from the root to the most recent leaf. The first line is the session's own entry, whose `id` is the session id, and every other entry descends from it. Session ids are 1 to 64 ASCII letters, digits and dashes (the start time and 32 random bits, such as `20260927T123456Z-1a2b3c4d`), since they become parts of paths; a file is a session only when it is a regular file named after the id its first line gives, and a file in a newer format than the running harness understands is refused. Session files hold prompts, code and tool output, so they are readable only by their owner (files 0600, folders 0700). `/rewind` moves the active leaf, so rewinding creates a branch without deleting history; the move is itself an entry, so it survives a restart. The file is created with the first message and locked while a process uses it, so a second process refuses it; on a file system that cannot lock files, it is used unlocked with a warning. Loading skips unreadable lines with a warning, stops at a missing parent or a loop in the entries with a warning, and resuming removes an incomplete last line before appending. Providers reject a tool call without a result, so a call that a killed run left without one gets one when the session is loaded, saying harness stopped before it finished and its effects are unknown. `-c` continues the project's most recently used session; until the terminal UI's picker, `--resume` without an id (and without a subcommand) lists the sessions (id, start time, first message) and `--resume <id>` continues one.

Auto-compaction triggers when the next request would reach 80% of the model's context window (configurable), and keeps the most recent turns verbatim within a budget (default 20% of the window); the summary is displayed to the user, stored as a `compaction` entry, and the originals stay on disk so the user can rewind to before the compaction. The settings are `[compaction] threshold_percent` and `keep_recent_percent`. The size of the next request is the input tokens the provider reported for the last one plus estimates for the messages since, the reply included, or else an estimate of the whole request; output tokens, reasoning included, are not sent back, so they do not count. The session's model writes the summary from a short summarizing prompt and a transcript of the older messages, each clipped (an earlier summary only to the whole transcript), with the oldest dropped to fit half the window; a summary request the model rejects as too long is sent once more with half that, and a summary cut off at the model's output limit is rejected. When no recent part fits the budget, automatic compaction keeps the current turn, or only its last step (the last model reply with its tool results) when the turn itself reaches the threshold; overflow compaction keeps the last step; and `/compact` summarizes everything. Summarizing nothing but an earlier summary counts as nothing to compact, and once an automatic compaction leaves the request at or over the threshold, automatic compaction waits until it drops below, so it never repeats without shrinking anything. A context overflow is recognised from the error text of an HTTP 400, 413 or 422 response, or of an error the provider reports inside its stream; never from a response harness could not parse, which quotes what it received and can hold the model's own text. Until model profiles report the real window, harness assumes 32,768 tokens.

### D8. Inline terminal UI

ratatui inline viewport (as in Codex): a live region at the bottom (input, streaming output, approval prompts), with finished messages inserted into normal terminal scrollback; only the live region is redrawn, so tmux and scrollback stay intact. Markdown via `pulldown-cmark`, code highlighting via `syntect`, diffs via `similar`, fuzzy file completion via `nucleo`. Pickers, `/rewind`, and long diffs use a temporary full-screen view and return to inline.

- **Steering:** input submitted while a turn runs is queued and sent when the turn ends; the send-now key (default Ctrl+S; raw mode disables XON/XOFF) delivers it at the next tool boundary instead.
- **Notifications:** when a turn that ran longer than 10 seconds finishes, or an approval is needed, harness emits an OSC 9 desktop notification and a terminal bell (configurable).
- **Pastes** over 10 lines or 1,000 characters collapse to a `[Pasted text #n, N lines]` placeholder that can be expanded for editing; the full text is sent.
- **Stats:** after each turn, a dim line shows the model, time to first token, output tokens per second, and prompt-cache hit rate when reported.
- `NO_COLOR` is honoured; no ANSI output when stdout is not a TTY.

### D9. Configuration and workspace trust

Paths follow the XDG base-directory spec on macOS and Linux: config `$XDG_CONFIG_HOME/harness` (default `~/.config/harness`), data `$XDG_DATA_HOME/harness` (default `~/.local/share/harness`: sessions, checkpoints, trust list, credential fallback), state `$XDG_STATE_HOME/harness` (default `~/.local/state/harness`: logs, tool output). `HARNESS_HOME`, when set, overrides all three with subdirectories of one path.

TOML layering: global `config.toml` ← project `.harness/config.toml` ← CLI flags. Project settings that could widen the harness's reach (a `mode` that grants more than the effective global mode, a `max_steps` above the effective global limit, `model`, `[permissions].allow`, `read_dirs`, provider definitions or `base_url` overrides, and any `[sandbox]` setting except `allow_localhost = false`) are applied only after the user trusts the workspace. The effective global mode is the global config's `mode`, else the default for the workspace (`auto` in a git work tree, `ask` elsewhere), and modes rank `plan` = `read-only` < `ask` < `auto` < `full-access`; the effective global limit is the global `max_steps`, else 50. Trust is stored in the data directory as a fingerprint (SHA-256) of those settings; if they change, the workspace is untrusted again. A workspace with none can be trusted too (the fingerprint of the empty set), so that its project command files may choose their model. `harness trust` shows the settings and records trust; the interactive first-use prompt arrives with the terminal UI. Headless runs ignore untrusted widening settings with a warning. Narrowing settings (`deny`, `confirm`, a mode or `max_steps` no wider than the effective global one, and `allow_localhost = false`) always apply.

Example global config:

```toml
model = "ollama/qwen3-coder:30b"
mode = "auto"

[providers.openrouter]
protocol = "openai-chat"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"

[profiles."ollama/qwen3-coder*"]
context_window = 65536
temperature = 0.2

[permissions]
allow = ["bash:cargo test*", "bash:git status*"]
deny  = ["bash:git push*"]
confirm = ["bash:terraform apply*"]
```

Built-in providers need no config: `ollama`, `lmstudio`, `llamacpp` (discovered), `openai`, `anthropic`, `openrouter` (API keys), `chatgpt` (sign-in).

### D10. Headless mode and exit codes

`harness ask "<prompt>"` appends piped stdin to the prompt, prints the final assistant text to stdout (progress to stderr), or the full event stream as NDJSON with `--json`. Actions that need approval are denied and the model is told why. Exit codes: `0` success, `1` runtime error, `2` invalid usage or no usable model, `3` finished with at least one action blocked for lack of approval, `130` interrupted.

### D11. Engineering baseline

MSRV pinned to the stable toolchain at M1 start via `rust-toolchain.toml`. CI on macOS and Ubuntu runners: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo nextest`, `cargo deny check`, and security suites on every PR (sandbox escapes, compound-command bypasses, symlink/path escapes, untrusted project config). Provider adapters are tested against recorded SSE fixtures (`insta` snapshots); the end-to-end suite runs `harness ask --json` against a local mock HTTP server in a temp git repo (`assert_cmd`, `tempfile`). Live-API tests are `#[ignore]` and run manually or nightly. The base system prompt is versioned in the repository with a changelog, so behaviour changes are visible.

### D12. Plan mode

`plan` is an approval mode with read-only permissions plus a short planning instruction appended to the conversation (not the system prompt, see D15). The turn ends with a plan message. The user can then choose **Build** (switch back to the previous non-plan mode and send "Implement the plan above"), **Edit** (open the plan in `$EDITOR`; the edited text is presented again for approval), or **Keep planning**. The approved plan is stored as a `plan` session entry. M2 attaches a different model to the build step (planner/builder split).

### D13. Checkpoints and rewind

Before the first mutating action of each turn (`write`, `edit`, or `bash` outside `plan`/`read-only`), harness snapshots the workspace into a shadow git repository at `$XDG_DATA_HOME/harness/checkpoints/<project-key>.git`, using a separate `GIT_DIR` so the user's own repository, index, and history are never touched and non-git directories work too. Snapshots honour `.gitignore` plus built-in excludes (`.git`, `node_modules`, `target`) and skip files over 10 MB. `/rewind` (or Esc Esc on empty input) lists previous user messages; the user picks one and restores **code and conversation**, **code only**, or **conversation only**. A rewind first snapshots the current state, and the rewind list then offers "undo last rewind" until something else happens in the session. Rewind cannot undo effects outside the workspace (network calls, databases, pushed commits), and the UI says so. Checkpoints use the `git` binary; when it is missing, checkpoints are disabled with a warning.

Each session has its own index (seeded from the last one written) and ref, `refs/harness/<session-id>`. git runs with the user's global and system configuration ignored, so no configured filter, hook or file-system monitor runs; the user's global excludes file does not apply either. Files that become git-ignored leave later snapshots. A restore runs `git read-tree --reset -u` on the target snapshot minus every path that exists in the workspace but is missing from the snapshot taken just before, so files that snapshots leave out (large or ignored) are never overwritten or deleted. The snapshot is taken after the action is approved, just before the tool runs; if it fails for any reason, checkpoints are disabled for the session with a warning and the turn goes on.

*Alternative:* per-file backups on `write`/`edit` only (Claude Code's approach). Rejected because it misses changes made by `bash`, which is how agents run formatters, code generators, and `rm`.

### D14. Local-model robustness

- **Effective context:** harness queries the context size the server is actually running with (llama.cpp `/props`, Ollama running-model info, LM Studio model info) and uses the smaller of that and the profile. If it is below the profile's `min_context` (default 32k tokens for agentic use), the user is warned with the fix (e.g. `OLLAMA_CONTEXT_LENGTH`).
- **Text tool calls:** when `text_tool_calls` is on (default for local providers), an assistant message with no native tool calls whose content is a recognised tool-call wrapper (`<tool_call>…</tool_call>` blocks, or a message consisting solely of a JSON object with `name` and `arguments`) is parsed into tool calls. Parsed calls go through the same schema validation.
- **Truncation:** when the provider reports output stopped at the length limit, a partial tool call is never executed; the model is told its output was cut off and asked to continue in smaller steps.
- **Bounded repair:** invalid tool calls are fed back as errors; the per-turn invalid-call count is exposed for M2's escalation rule.

### D15. Cache-stable prompt prefix

Local servers and hosted providers both reuse cached prompt prefixes, and cache misses dominate latency on Macs. Within a session, the system prompt and tool definitions are byte-identical across turns: the date and git status are captured once at session start, tool definitions keep a fixed order, and instruction files are read at session start. Mode changes, plan instructions, and similar context are appended as messages rather than edited into the prefix (a mode change appends `[harness] The approval mode is now …`). Only compaction or a model switch rebuilds the prefix. Conversation history is append-only on the active branch. A session resumed in a new process reads the instruction files and captures the environment again. On macOS the environment section also tells the model to write multi-line commit messages with `git commit -F - <<'EOF'`, because `/bin/bash` 3.2 rejects `git commit -m "$(cat <<'EOF' … EOF)"` whenever the message has an odd number of quotes or backticks.

### D16. Shell command analysis

`harness-shell` parses commands with `brush-parser` (pure Rust, bash grammar) and walks lists, `&&`/`||`, pipelines, subshells, brace groups, and command substitutions (recursively, depth ≤ 8). Words are split into literal and dynamic tokens after quote removal (`$'…'` decoded); a dynamic token only matches a `*` in a rule. Wrappers (`command`, `builtin`, `exec`, `nohup`, `time`, `nice`, `timeout`, `stdbuf`, `env`, `sudo`, `sh|bash -c`, `eval`, `xargs`, `find -exec`) are unwrapped so deny rules and destructive detection see the inner command; path prefixes and leading backslashes are stripped from command names. Rules match the shell-quoted argv. Loops, conditionals, functions, process substitution, parse errors and inputs over 10,000 characters are undecomposable and always prompt (after a best-effort deny/destructive scan). Inputs over 262,144 bytes are not scanned at all, which bounds the scan's time and memory; they prompt as possibly hiding a denied command whenever a deny rule exists, so `full-access` prompts for them too. bash 3.2 (macOS `/bin/bash`) does not parse a command substitution as a program: it finds the end of a `$(…)` by counting parentheses and pairing quotes (`parse_matched_pair`), with no notion of here-documents or `${…}`, and finds it again with other comment rules when it expands the word (`extract_delimited_string`). So on every platform, each program bash parses from its start (the command, `bash -c` and `eval` text) is also read as bash 3.2 reads it: each `$(…)` in it, and in its here-document bodies, `${…}` values and backquoted commands, must end at the same `)` in both bash 3.2 readings and in brush-parser's, and so must the text bash 3.2 runs from it. Where an assignment may start a command, bash 3.2 reads the subscript of `name[…]` as text, so brush-parser must end that word at the same `]`, and the substitutions bash 3.2 expands from the subscript are checked too. brush-parser is checked on each construct's own text, and in the program only for the word the construct lies in; it can still read a construct differently in context, which is a limit of the check, not a guarantee (the comment checks cover the cases known). Each substitution the analysis takes must also end where bash 5, which sees a comment at the start of any word, ends it. A `shopt` that sets or unsets an option, and `bash -O`, are undecomposable too: the analysis parses with `extglob` off and expands no aliases. A disagreement is undecomposable and roughly scanned, so deny still wins; bash 3.2 reaching the end of the input inside a construct (a syntax error, after which nothing more runs) or reporting a bad substitution is accepted. The body of a here-document with an unquoted delimiter is joined across backslash-newlines, as bash reads it, before its substitutions are analysed. So is `git` with `-c`, `--config-env` or `--exec-path`, since command-line configuration can run programs, except a `-c` whose key cannot: `user.name`, `user.email`, `init.defaultBranch`, `color.*`, `advice.*`, `core.quotepath`, and `commit.gpgsign`/`tag.gpgsign` set to `false` (keys match in any case, as git's do). Destructive detection understands git global options, long-option abbreviations and short-flag clusters (`push -f/--force*/--mirror/--delete/+ref`, `reset --hard`, `clean` without `-n`, `checkout -f`/pathspecs, `restore` of the worktree) and recursive `rm` whose target is the workspace root, an ancestor, or outside it, tracking `cd` within the command.

### Design principles adopted from the research

- **No telemetry.** Network traffic goes only to configured providers and localhost discovery.
- **No silent model switching.** In M1 only the user changes models; every message shows which model wrote it.
- **Never remove a loved feature without a replacement** (Codex `/undo` removal and Amp's thread removal drew sustained backlash).
- **Ask with real stakes only.** The sandbox handles routine safety so approval prompts stay rare enough to be read.

## Risks / Trade-offs

- [OpenAI withdraws tolerance for ChatGPT sign-in in third-party tools] → Isolated module behind a feature flag; API keys and M3's `codex app-server` delegation remain as sanctioned paths.
- [Anthropic policy shifts; the Claude client may fingerprint wrapped use] → M1 does not touch Claude subscriptions; M3 re-checks the policy before implementation.
- [Apple removes `sandbox-exec`] → Sandbox is behind a `harness-sandbox` trait; on failure, harness degrades to approval-required for every command, never silently unsandboxed.
- [Landlock/seccomp unavailable (old kernels, some containers)] → Same degradation plus a clear startup warning.
- [Unprivileged user namespaces blocked (stock Ubuntu 24.04+, Docker, CI)] → Linux falls back to the basic tier: git metadata is protected only after the fact, by the guard's quarantine and restore. A startup warning points to `harness sandbox doctor` and its one-line fix, and `sandbox.linux_git_protection = "required"` turns the basic tier into approval for every command.
- [Shell parsing misses an exotic construct] → Anything the parser cannot fully decompose requires approval; the sandbox remains the second line of defence; bypass attempts are part of the security test suite.
- [Checkpoints are slow or large in big workspaces] → Excludes and size cap; snapshot only before the first mutating action per turn; if a snapshot exceeds 5 seconds, checkpoints for that session are disabled with a warning.
- [Text tool-call parsing misfires on example code] → Only whole-message wrappers are recognised, parsing is off by default for hosted providers, and every parsed call is schema-validated and permission-checked.
- [Local servers run with a smaller context than the model supports] → D14 effective-context detection and warning.
- [Malicious repository content: command files, project config, instruction files] → Command shell expansion uses the same approval and sandbox as `bash`; `allowed-tools` cannot override deny rules or destructive-command confirmation; widening project settings need workspace trust; imports are confined to the discovery root.
- [Supply-chain risk in dependencies] → `cargo-deny` advisories and licence checks in CI; lockfile committed.
- Trade-off: inline rendering limits rich layouts (split panes). Accepted in favour of native scrollback, copy, and search; full-screen views are used only where they add value.
- Trade-off: M1 grew by roughly a third after the research round. Accepted because plan mode, checkpoints, permission hardening, XDG paths, and the cache-stable prefix all shape core data structures and are costly to retrofit.

## Migration Plan

Not applicable (greenfield). Releases start as `0.x` pre-releases built from source (`cargo install --path crates/harness-cli`); packaged distribution is M5.

## References

Vendor policy and integration surfaces:

- Anthropic, Claude Code legal and compliance: https://code.claude.com/docs/en/legal-and-compliance
- Anthropic, Agent SDK overview: https://code.claude.com/docs/en/agent-sdk/overview
- Anthropic, Agent SDK with Claude plans (policy status): https://support.claude.com/en/articles/15036540-use-the-claude-agent-sdk-with-your-claude-plan
- OpenAI, Codex app-server: https://learn.chatgpt.com/docs/app-server
- OpenAI, Codex authentication: https://learn.chatgpt.com/docs/auth
- OpenAI (Codex team) on third-party harnesses: https://x.com/thsottiaux/status/2058071172361998482
- Gemini CLI terms: https://geminicli.com/docs/resources/tos-privacy/

Harnesses and patterns:

- vercel-labs/fx: https://github.com/vercel-labs/fx
- openai/codex: https://github.com/openai/codex
- pi: https://github.com/earendil-works/pi
- OpenCode commands: https://opencode.ai/docs/commands/
- Capy planner/builder split: https://capy.ai/blog/captain-vs-build
- Aider architect/editor: https://aider.chat/2024/09/26/architect.html
- Amp handoff and "The Dial": https://ampcode.com/news/handoff, https://ampcode.com/news/the-dial
- Kiro specs and hooks: https://kiro.dev/docs/specs/, https://kiro.dev/docs/hooks/
- Claude Code checkpointing and agent view: https://code.claude.com/docs/en/changelog, https://code.claude.com/docs/en/agent-view
- Goose removal of automatic model switching: https://github.com/aaif-goose/goose/issues/5781
- Claude Code sandboxing (84% fewer prompts): https://www.anthropic.com/engineering/claude-code-sandboxing
- claude-code-router: https://github.com/musistudio/claude-code-router
- Agent Client Protocol: https://agentclientprotocol.com

Local models:

- Ollama context length: https://docs.ollama.com/context-length
- Local agent tuning case study (cache hits, output caps): https://doug.sh/posts/tuning-a-local-coding-agent-oh-my-pi/
- Prompt-cache invalidation by changing headers: https://github.com/musistudio/claude-code-router/issues/1217
- Ollama Anthropic compatibility: https://docs.ollama.com/api/anthropic-compatibility

Developer sentiment and demand:

- Stack Overflow Developer Survey 2025, AI section: https://survey.stackoverflow.co/2025/ai
- JetBrains AI coding agent adoption 2026: https://blog.jetbrains.com/research/2026/08/ai-coding-agent-adoption-2026/
- Pragmatic Engineer AI tooling 2026: https://newsletter.pragmaticengineer.com/p/ai-tooling-2026
- Most-requested features: Claude Code #6235 (AGENTS.md), #18435 (accounts), #1455 (XDG); Codex #2109 (hooks), #9203 (undo), #2101 (plan mode), #8745 (LSP); OpenCode #7602 (fallback), #6231 (model discovery)
