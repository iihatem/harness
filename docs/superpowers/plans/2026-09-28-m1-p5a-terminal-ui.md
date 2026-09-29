# M1 · P5a Terminal UI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give harness its interactive terminal, the half of P5 that does not need P4. `harness` without a subcommand (and `harness -c`, `harness --resume <id>`) opens an inline session: the conversation goes into the terminal's own scrollback, and only a live region at the bottom (the input, what streams, prompts) is redrawn. Replies render as Markdown with highlighted code, file changes as diffs. The input editor has history, multi-line input, collapsed pastes, and `/` and `@` completion. A status line, per-turn stats, `/context` and `/usage` show where the tokens go. Approvals are answered at the terminal, with diffs, the re-run-outside-the-sandbox offer and Shift+Tab mode cycling. Input typed during a turn is queued or, with Ctrl+S, steered into the running turn. Plan mode ends with Build, Edit (in `$EDITOR`) or Keep planning. Long turns and approvals send an OSC 9 notification and a bell. A workspace's untrusted widening settings are asked about on first use. Two follow-ups from 2.13's final review come along: a Linux session that drops to the basic tier with `"required"` asks before running outside the sandbox (M1), and the dead `save_all = false` path goes (M4).

**Architecture:** A new `harness-tui` library crate holds the terminal UI: rendering (`markdown`, `highlight`, `diff`, `text`, `style`), its own inline terminal on ratatui's backend and buffers (`inline`), the transcript that turns the agent's events into scrollback lines (`transcript`), the input editor and completion (`editor`, `complete`), the status line and reports (`status`), approvals (`approval`), plan mode's ending (`plan`), notifications (`notify`), the terminal's modes (`terminal`), the app's state machine (`app`), and the session loop (`ui`), which runs the agent in a task of its own. `harness-core` gains a `TurnStats` and a `Steered` event, `Agent::context_usage`, approval kinds, per-mode sandboxes (`Sandboxes`), a `Steering` handle, the approved plan on the Build turn's message entry, and `CommandSandbox::cannot_run`. `harness-cli` moves the agent's startup out of `ask.rs` into `start.rs`, shared by `harness ask` and the new `interactive.rs`, which sets up the real terminal and asks the first-use trust question. `harness-config` reads `[notifications]`.

**Tech Stack:** Rust 1.98, edition 2024; `ratatui` 0.30 (backend, buffers and widgets; not its inline viewport) on `crossterm` 0.29 (raw mode, bracketed paste, the kitty keyboard protocol, `EventStream`); `pulldown-cmark` 0.13 for Markdown; `syntect` 5.3 (pure-Rust regexes, bundled syntaxes and themes) for code; `similar` 3.2 (already in the workspace) for diffs; `nucleo-matcher` 0.3 and `ignore` for `@` completion; `unicode-width` for columns; tests with ratatui's `TestBackend`, scripted keys and pastes, `harness_core::testing::MockProvider`, and `tempfile`.

**Spec:** `openspec/changes/add-core-agent/` (binding): `design.md` D1, D5, D7, D8, D9, D10 and D12; `specs/cli-interface` (inline rendering, status line, keys, notifications, pastes), `specs/plan-mode`, `specs/agent-runtime` (steering, the event stream), `specs/permissions-sandbox` (approval prompts, the basic tier with `"required"`), `specs/slash-commands` (`/context`, `/usage`, completion) and `specs/configuration` (the first-use trust prompt). Task 1 writes this plan's refinements (below) into `design.md`, five specs and `tasks.md`.

## How this plan was checked

Every task was built in order, as its own commit, test first, in a scratch clone of this repository: `/private/tmp/claude-501/-Users-mac-Desktop-dev-AI-harness/78b07910-5be3-48d0-b7e8-1da36ddc200d/scratchpad/p5a-seq`, branch `p5a`, on `f2cc9fb`. The commits are `T1` to `T14`, one per task: `T1` e2d415f, `T2` 0882267, `T3` c6f7d47, `T4` b7b0fd9, `T5` 438f928, `T6` f0d370b, `T7` e5957b8, `T8` aa7c56c, `T9` ef9458b, `T10` 4091af7, `T11` 646959e, `T12` 18663ba, `T13` 4cd4fba, `T14` f9480bc. Implementers may copy from them (`git -C <clone> show <commit>:<path>`), checking what they copy against this plan. Each commit was checked on its own, on macOS:

- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `openspec validate add-core-agent --strict`;
- `cargo test --workspace`: 1,008 tests pass at `f2cc9fb`, and after each task 1,008, 1,016, 1,025, 1,032, 1,036, 1,048, 1,058, 1,070, 1,079, 1,086, 1,094, 1,102, 1,106 and 1,107. One run at Task 4 had a single failure in `harness-sandbox` (`gitmeta::linked::tests::the_entries_on_the_way_are_deduplicated_in_linear_time`, a timing test from 2.13, `289ms against 35.85ms`) while another build loaded the machine; Task 4 touches only `harness-tui`, and the test passed when the run was repeated;
- Step 2 of every task: the task's tests were put on the commit before it, and the failures listed under "Expected" are the ones they produced;
- `cargo deny check` after Tasks 2, 5, 6 and 14: advisories, bans, licences and sources pass (decision 3 is the one advisory ignored); the only new duplicate version is `fancy-regex` (0.16 for `syntect`, 0.19 already in the tree);
- the Linux code in Task 14 (`LinuxSandbox::cannot_run`, its unit test, and the guard's call site) was linted for `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu` with the lint probe 2.13's plan describes (a copy of `harness-sandbox` next to a stand-in `harness-core`);
- the plan itself was replayed task by task onto a fresh clone at `f2cc9fb`, and every task's result matched its verified commit (`Cargo.lock` aside, which cargo writes);
- the fourteen commits were also moved onto P4's scratch branch as it stood at `df713ae` (P4's Tasks 1 to 10 and its plan): see "Before you start".

Beyond the tests, the real binary was driven in a pseudo-terminal (Python's `pty`, with the `pyte` VT100 emulator answering the cursor-position queries and showing the screen) against a mock OpenAI-chat server, with a temporary `HARNESS_HOME`: a prompt and its reply in the scrollback; a 30-line bracketed paste collapsed and sent whole; an approval in `ask` mode (with the OSC 9 bytes and the bell checked); Shift+Tab to `plan`; plan mode's Edit with a real `$EDITOR` command and Build; Ctrl+S steering into a running `sleep` (Ctrl+S arrives as a key, since raw mode clears `IXON`); and the first-use trust question. That script is not part of the repository.

What was not verified:

- **A person at a real terminal.** Only a pseudo-terminal and an emulator saw the output. Not checked by a person: how it looks in iTerm2, Terminal.app, kitty, WezTerm, Ghostty, GNOME Terminal or tmux; the syntax theme's colours on a light background; Shift+Enter under the kitty keyboard protocol (the pseudo-terminal answered that it has none); whether each terminal shows OSC 9 (tmux passes only the bell unless `allow-passthrough` is on); how scrollback reflows when a terminal is resized mid-turn; editors that take over the terminal (`vim`, `nano`) during plan mode's Edit; and keys typed very fast right before Edit, which crossterm may already have read.
- **Linux.** The interactive session never ran on Linux, and Task 14's Linux path (`cannot_run`, and the agent asking after a drop to the basic tier) was linted, not run: its unit test runs in the pull request's CI. `harness-core` pulls in `aws-lc-sys`, so the workspace cannot be linted for Linux from macOS.
- **P4.** P4 is not merged. The move onto P4's branch shows where the replay conflicts and that it builds, but P4 may still change, and the P4-dependent parts (model profiles' window in `/context`, redaction of what the terminal prints, `--debug` for the session) are listed, not built.
- **Real providers.** No request reached a real provider; the mock provider and the mock server answered everything.

## Before you start: P4, and the replay onto master

P5a was built on `f2cc9fb` (P3 merged), before P4, by the human's choice: P4 is planned in parallel (`docs/superpowers/plans/2026-09-28-m1-p4-providers.md` on P4's branch) and not merged. Every `Replace` block below matches `f2cc9fb`. Whichever merges second meets the conflicts below: this plan replayed onto `master` after P4, or P4 after this plan. They are the ones met when the fourteen commits were moved onto P4's scratch branch at `df713ae`; resolved as below, that branch built, `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings` were clean, and `cargo test --workspace` passed 1,234 tests (P4 alone: 1,135).

| Task | File | What P4 changed | Resolution |
|---|---|---|---|
| 1 | `design.md` (D9), `specs/configuration/spec.md` | model `[profiles]` join the widening settings | make this plan's sentence change in P4's paragraph |
| 2, 5, 6 | `Cargo.lock` | P4's crates | take P4's and let cargo add these crates |
| 6 | `crates/harness-cli/Cargo.toml` | `[features]`, `nix` with `term` | keep both |
| 6 | `crates/harness-cli/src/main.rs` | `mod auth; mod login;` | keep both |
| 6 | `crates/harness-cli/src/ask.rs` | the model setup this task moves to `start.rs`: the debug log, model profiles, the window local servers run, the local-to-hosted warning, request options, text tool calls, the redactor | see below |
| 6 | P4's new `session_flags_with_the_credential_commands_are_refused` in `crates/harness-cli/tests/cli_smoke.rs` | expects the old refusal text | expect Task 6's new refusal text, as its `cli_smoke.rs` step does |
| 7 | `crates/harness-core/src/agent.rs` | the `redactor` field | keep both fields |
| 10 | `crates/harness-core/src/agent.rs` | `after_cut_off` where `deliver_steering` goes | keep both methods |
| 11 | P4's `crates/harness-core/tests/redact.rs` | builds an `EntryKind::Message` | add `plan: None,` (it compiles no other way) |
| 12 | `crates/harness-config/src/config.rs` | `ProfileSettings` and the `profiles` fields where `[notifications]` goes | keep both, and give `ConfigFile::notifications` its own `#[serde(default)]`: git's merge keeps one attribute line for the two fields, and a config without `[notifications]` then fails to parse (two tests catch it) |
| 14 | `README.md` | P4's status line and roadmap row | say P5a's first half is in, as this plan's text does |

Task 6 with P4: `start.rs` takes P4's model setup, in the place `ask.rs` had it.

- `Request` gains `run_id: String` and `cancel: CancellationToken`, and `start` returns `Option<Started>`: `None` when Ctrl+C arrives while a local server is asked for its window (`ask` then exits 130; the interactive session passes a token nothing cancels). A new `pub fn run_id() -> String` makes the id; `ask` passes it, and names `--debug`'s log with it, as P4's `ask.rs` did.
- After `tool_context`: P4's profile, `window::running_context` in a `select!` with `cancel`, `effective_window` and its warnings, and the local-to-hosted warning (its `model_is_local` moves to `start.rs` too).
- `crate::context::system_prompt(setup, &prompt::base_prompt(mode, sandboxed, interactive), context_window)`; then `config.context_window = context_window`, `config.request = profile.request_options()` and `config.text_tool_calls = profile.text_tool_calls`; `Agent::new(…).with_redactor(setup.redactor.clone())`. `start::context_window`, this plan's stand-in for the window, goes.
- In `interactive.rs`, resolve the model with `setup.keys()`. P4 made `registry::resolve` take any `Secrets`, and a plain function such as `setup::env` still compiles but never looks at stored keys.
- Tasks 8 and 9 then meet only what this resolution changed (the `let Some(Started { … })` in `interactive.rs`, and where Task 9's new session start goes in `start.rs`: before P4's model setup).

What no compile error shows, to do in the replay or the full P5 plan:

- **Redaction.** P4 replaces secrets in everything harness prints. The interactive session prints model and tool text through its transcript, so it must pass each event through `setup.redactor` before the app takes it in.
- **`--debug`** is a global flag in P4; the interactive session should write the same log.
- **The context window** comes from P4's profiles and servers: `Agent::context_usage` and so the status line and `/context` follow it, but `/context`'s "assumed until model profiles report the model's own" note (the `window_note` option in `interactive.rs`) should go or say where the window came from.
- **P4's messages that point at the terminal UI** (a quota error suggesting `/model`) name commands that arrive only with the rest of P5.
- **Steering after tool results.** P4's decision 5 notes that a turn ending on tool results followed by a new prompt reads as two user turns to some chat templates; steering puts the user's text right after tool results in the same turn, which P4's Anthropic adapter joins into one user message and the Chat Completions adapters send as they are.

If a `Replace` block in `ask.rs`, `main.rs`, `agent.rs`, `config.rs`, `context.rs`, `slash.rs` or `README.md` does not match exactly, P4 changed a nearby line: make the same change by hand next to P4's code, as above.

## Decisions this plan asks you to approve

The spec settles what P5a does; these settle how, where it is silent. Task 1 writes all of them into it except the crate versions in 2, 3 (a `deny.toml` entry) and 18 (clean-up).

1. **A new crate, `harness-tui`,** holds the terminal UI; `harness-cli` sets up the session and the real terminal and starts it. D1 put the inline TUI in `harness-cli`. A library can be tested from its own `tests/` with the mock provider and `TestBackend`, and keeps P5 out of the files P4 rewrites. Codex has the same split (`codex-tui`). *Alternative:* a `tui` module in the binary, tested only from inside it.
2. **New third-party crates**, each checked with `cargo deny check`:
   - `ratatui` 0.30.2 (MIT; default features off, `crossterm` on): buffers, widgets, the backend trait and `TestBackend`;
   - `crossterm` 0.29.0 (MIT; `event-stream`): raw mode, bracketed paste, keyboard enhancement, the key stream. Already under `ratatui`, now named directly by `harness-cli`;
   - `pulldown-cmark` 0.13.4 (MIT; default features off): Markdown;
   - `syntect` 5.3.0 (MIT; default features off, with `parsing`, `default-syntaxes`, `default-themes` and `regex-fancy`, so no Oniguruma C library): code highlighting. It brings `fancy-regex` 0.16 next to the 0.19 already in the tree;
   - `nucleo-matcher` 0.3.1 (MPL-2.0): fuzzy `@` completion. D8 names `nucleo`; `nucleo-matcher` is its matcher without the thread pool, enough for up to 20,000 paths;
   - `unicode-width` 0.2.2 (MIT OR Apache-2.0): screen columns (already in the tree under `ratatui`).

   `Cargo.lock` gains 56 packages, and the binary syntect's syntaxes and themes.
3. **`cargo deny` ignores RUSTSEC-2025-0141** (bincode 1.3.3 is unmaintained, and its team calls it complete), with the reason in `deny.toml`. `syntect` decodes with it only the syntax and theme dumps built into harness, never outside input. *Alternative:* no syntax highlighting until syntect moves off bincode.
4. **harness keeps its own inline terminal** (`InlineTerminal`) on ratatui's `Backend` and `Buffer` rather than ratatui's `Viewport::Inline`, whose height is fixed when the terminal is made. The live region grows and shrinks with what it shows (a multi-line input, a completion list, an approval with a diff); finished lines are written above it and pushed into scrollback by scrolling the screen; only changed cells are written. Codex does the same.
5. **The agent reports two more events: `turn_stats`** before `turn_finished` (the model that answered last, time to first token, streaming time, input, output and cached tokens), **and `steered`** when send-now input reaches the model. D1 lists `TurnStats`, and the agent-runtime spec's event stream covers per-turn statistics. `harness ask --json` prints them too (a `turn_stats` line in every turn that called the model; `steered` never occurs headless); plain `harness ask` output does not change. *Alternative:* the UI measures stats itself, and headless output stays as it is.
6. **Steering.** Enter during a turn queues the input for a new turn when this one ends. Ctrl+S puts it in a `Steering` handle the agent reads after each step's tool results, adding it to the conversation as a user message of the same turn. Send-now input that no tool result took before the turn ended starts the next turn, before queued input. After Esc, queued and unsent send-now input go back into the editor instead. Built-in commands that need no turn (`/help`, `/context`, `/usage`) run at once; a slash command cannot be sent now, only queued.
7. **Mode switching.** Shift+Tab switches between turns, and during a turn picks the mode to switch to when the turn ends (the status line says which). From `read-only` it goes to `ask`, from `full-access` to `plan`. The agent takes a sandbox per access (`Sandboxes`): `plan` and `read-only` keep a read-only sandbox where `ask` and `auto` have none (a workspace too broad to make writable, the Linux basic tier with `"required"`), and `full-access` none. So the interactive session looks for a sandbox whatever mode it starts in, `full-access` included; `harness ask`, which never switches, looks as before.
8. **Approvals.** `y` once, `a` for the session, `n` to deny with an optional reason typed for the model, Esc (or Ctrl+C) to deny and stop the turn. A `write` or `edit` shows a unified diff against the file as it is on disk (read up to 1 MB, text only; shown only to the user); a long diff scrolls inside the prompt (Up, Down, PgUp, PgDn), since the full-screen view comes with the pickers. The offer to run a command outside the sandbox has no session choice. A session approval the policy cannot keep (a destructive command, one it always asks about) now warns that it applied once.
9. **Plan mode.** The mode note for `plan` asks the model to investigate, then end its reply with a step-by-step implementation plan; an interactive session started in `plan` gives that note with its first message. A completed planning turn shows Build, Edit and Keep planning. Build goes back to the mode before `plan`, or, for a session started in `plan`, the configured mode unless it is `plan` or `read-only`, else the workspace's default; it sends "Implement the plan above." (after an edit, "Implement this plan:" and the edited plan). Edit runs `sh -c '$EDITOR "$1"'` (`vi` without one) on a private temporary file, with the terminal's modes undone meanwhile. The approved plan is saved as a `plan` field on the Build turn's message entry, which older harness versions ignore, so no session format change. Headless `--mode plan` stays as it is, without the planning note or the choices. *Alternative:* a `plan` entry kind with a session format bump.
10. **The editor's keys.** Enter sends; Alt+Enter, Shift+Enter (where the terminal reports it), Ctrl+J, or a `\` before Enter make a new line; Up and Down move between rows, then recall earlier inputs: this session's, seeded from a resumed session's user messages (not kept across sessions); Ctrl+O expands a collapsed paste (backspace deletes one whole); Ctrl+D on empty input exits; readline's Ctrl+A, Ctrl+E, Ctrl+W, Ctrl+U, Ctrl+K and Alt+B, Alt+F. A recalled long input comes back collapsed.
11. **Built-in commands in P5a:** `/help`, `/context`, `/usage`, `/quit`, `/init` and custom commands. `/model`, `/mode`, `/new`, `/resume`, `/rewind`, `/compact` and `/login` say they are not available yet and when they come; Esc Esc does nothing yet. `/usage` counts what providers reported since harness started (sessions do not store usage). `/context` splits the window into the system prompt, tool definitions, each instruction file, the conversation and free space, from estimates.
12. **Notifications:** `[notifications] desktop` and `bell`, both on by default, set independently; a project may set them without trust, since they change nothing the agent may do. A turn notifies when it ran 10 seconds or longer and ended other than by the user's interruption; an approval always does. The text starts `harness:` (so no terminal reads it as a numbered OSC 9 command) and has its control characters removed.
13. **The first-use trust question** comes before the configuration loads, only when the workspace has widening settings that are not trusted. Yes records trust as `harness trust` does, so the settings apply at once; no leaves them off for the session and asks again next time.
14. **The interactive base prompt** says the user answers approvals (and that without a sandbox every shell command needs the user's approval), instead of headless's "actions that need approval will be refused". The system prompt still stays byte-identical within a session.
15. **Interactive mode needs a terminal** on both stdin and stdout; without one, `harness` exits 2 and names `harness ask`, as it does today. `harness -c` and `harness --resume <id>` without a subcommand continue a session interactively, and the refusal of `-c` with another subcommand says so.
16. **One startup for both frontends.** `ask.rs`'s agent setup moves to `start.rs`, used by `harness ask` and the interactive session; slash-command expansion returns its warnings and notes instead of printing them, since nothing may write to the terminal in raw mode. `harness ask`'s output and exit codes do not change, and its end-to-end tests pass unchanged. The window stays the assumed 32,768 tokens, behind `start::context_window`, the one function P4's window detection replaces.
17. **2.13 final review M1:** `CommandSandbox::cannot_run(access)` says when a sandbox would refuse a command (on Linux, a command that may write, once the session dropped to the basic tier with `"required"`). The agent then asks whether to run it outside the sandbox, once, as a session that starts in the basic tier does. A headless run counts it as blocked and exits 3, where it now fails the command and can exit 0.
18. **2.13 final review M4:** both tiers save every protected file, so the guard's `save_all` parameter, the snapshot's partial mode (symlinks and multiply linked files only) and the two tests of that configuration go; three tests are renamed.
19. **Colours.** `NO_COLOR` turns every colour off (bold, dim, italic and reverse stay, and inline code keeps its backticks); syntax highlighting uses syntect's `base16-ocean.dark` in 24-bit colour when `COLORTERM` says `truecolor` or `24bit`, else the nearest of the 256 standard colours.

## Global Constraints

- Rust `1.98.0`, edition 2024, licence `MIT OR Apache-2.0`, macOS and Linux; every crate keeps `publish = false`, and `harness-tui` uses the workspace's package fields.
- Commits: `git commit -F -` with a conventional-commit subject, a short body, and the trailer lines the controller gives you (written `<trailer lines from the controller>` below).
- Third-party crates: only those in decision 2; `cargo deny check` stays green with the one ignore in decision 3.
- `harness ask` keeps its behaviour and exit codes (`0`, `1`, `2`, `3`, `130`); its end-to-end tests pass unchanged. `--json` gains `turn_stats` lines (decision 5).
- No test needs a real terminal or a TTY: rendering is tested on ratatui's `TestBackend`, keys and pastes are scripted, and the terminal's modes and notifications are tested through traits (`RawMode`, `Notify`) and the bytes written.
- No test reads a real credential or calls a real provider: the agent runs against `harness_core::testing::MockProvider`.
- Everything drawn from the model, tools, files, the user or the configuration goes through `text::sanitize` (control and bidirectional characters shown as escapes); what the CLI prints still goes through `terminal_safe`.
- While raw mode is on, nothing writes to stdout or stderr but the inline terminal and the notifier: warnings and notes go into the transcript.
- The live region never grows past the screen's height; the system prompt stays byte-identical across a session's requests, and mode changes are notes in the conversation.

## Review Focus

- **Wide characters** (CJK, emoji) typed or pasted: the cursor and wrapping must count screen columns, not characters. Test in Task 4 (`wide_characters_take_two_columns_for_the_cursor_and_wrapping`).
- **Leaving while a turn runs** (Ctrl+C twice mid-reply): the turn must stop and the session end, not hang on the turn or leave the agent, and its session file, behind. Test in Task 6 (`quitting_while_a_turn_runs_stops_it_and_ends_the_session`).
- **Ctrl+C at an approval prompt** (the reflex for "stop"): it must answer the prompt no and stop the turn, since a turn waiting on an unanswered approval cannot stop. Test in Task 8 (`ctrl_c_at_a_prompt_denies_and_stops_the_turn`).
- **An approval arriving while the user is typing** their next message: the prompt replaces the input only while it waits; the draft is neither lost nor sent. Test in Task 8 (`a_draft_typed_before_an_approval_is_kept`).
- **A narrow or short terminal** (a split pane, a resized window) at a prompt with a long command: the reason and the keys must still show, wrapped, and the answer still work. Test in Task 8 (`a_narrowed_terminal_wraps_the_prompt_and_still_takes_the_answer`).

---

## File Map

Files P4 also changes are marked **P4**; see "Before you start".

```
openspec/changes/add-core-agent/{design.md,tasks.md,specs/*}   refinements (Task 1)          P4
Cargo.toml, deny.toml                    the new crates (Tasks 2, 5, 6); the bincode ignore   P4 (Cargo.toml)
crates/harness-tui/                      NEW crate
  src/style.rs, text.rs                  colours and NO_COLOR; sanitizing and wrapping (Task 2)
  src/markdown.rs, highlight.rs, diff.rs Markdown, code, diffs (Task 2)
  src/inline.rs, transcript.rs           the inline terminal; events into scrollback lines (Task 3)
  src/editor.rs, complete.rs             the input editor (Task 4); / and @ completion (Task 5)
  src/app.rs, ui.rs, terminal.rs         the app, the session loop, the terminal's modes (Task 6; Tasks 7 to 12)
  src/status.rs                          status line, turn stats, /context, /usage (Task 7)
  src/approval.rs                        approvals at the terminal (Task 8)
  src/plan.rs                            Build, Edit, Keep planning; $EDITOR (Task 11)
  src/notify.rs                          OSC 9 and the bell (Task 12)
  tests/{render,inline,editor,complete,session,status,approvals,modes,steering,plan,notify}.rs
crates/harness-core/src/
  event.rs                               TurnStats (Task 7), Steered (Task 10)
  agent.rs                               stats, context_usage, approval kinds, Sandboxes, steering,     P4
                                         the approved plan, running outside a dropped sandbox
  engine.rs, permission.rs               switching whether a sandbox is available (Task 9)
  turn.rs, session.rs                    Steering; TurnInput::plan; the entry's plan field (Tasks 10, 11) P4 (session.rs)
  tool.rs                                CommandSandbox::cannot_run (Task 14)
crates/harness-core/tests/{stats,approvals,mode_sandboxes,steering,plan,unsandboxed}.rs   NEW
crates/harness-config/src/config.rs      [notifications] (Task 12)                            P4
crates/harness-sandbox/src/              the guard without save_all; LinuxSandbox::cannot_run (Task 14)
crates/harness-cli/src/
  start.rs                               NEW  the agent's startup for ask and the TUI (Task 6)          P4 (ask.rs's setup)
  interactive.rs                         NEW  the interactive session on the real terminal (Task 6)
  ask.rs, main.rs, slash.rs, prompt.rs   startup moved out; `harness` alone; messages; interactive prompt  P4
  context.rs, sandbox.rs, trust.rs       instruction sizes; per-mode sandboxes; the first-use question   P4 (context.rs)
README.md                                the interactive terminal (Task 14)                    P4
```

---

### Task 1: Write the refinements into the spec

**Files:**
- Modify: `openspec/changes/add-core-agent/design.md`, `openspec/changes/add-core-agent/tasks.md`, and in `openspec/changes/add-core-agent/specs/`: `cli-interface/spec.md`, `agent-runtime/spec.md`, `plan-mode/spec.md`, `permissions-sandbox/spec.md`, `configuration/spec.md`

**Interfaces:**
- Consumes: nothing.
- Produces: the binding text for the decisions above, and this plan's path in `tasks.md`.

- [ ] **Step 1: Update design.md**

In `openspec/changes/add-core-agent/design.md` (D1, D5, D7, D8, D9 and D12):

Replace (1 of 4):

````markdown
  harness-sandbox    macOS Seatbelt; Linux Landlock + seccomp
  harness-context    AGENTS.md/CLAUDE.md discovery; prompt assembly; slash-command discovery and expansion
  harness-config     XDG paths, config layering, workspace trust
  harness-cli        `harness` binary: clap subcommands, inline TUI, `ask`, NDJSON output
```

The core runs on `tokio` and emits `AgentEvent`s: `TurnStarted`, `TextDelta`, `ReasoningDelta`, `ToolCallRequested`, `ApprovalNeeded`, `ToolCallFinished`, `Usage`, `TurnStats`, `Retrying`, `Compacted`, `CheckpointCreated`, `TurnFinished { reason }`, `Error { kind }`. Every assistant message carries the id of the model that produced it. Frontends send back `UserInput { delivery: Queued | SendNow }`, `ApprovalDecision`, and `Interrupt`. The TUI, `ask` (plain), and `ask --json` (NDJSON) are consumers; the M4 ACP server will be another.

*Alternatives:* a local daemon plus clients (OpenCode-style) enables shared live sessions but adds a daemon, ports, and auth, and OpenCode draws criticism for 1 GB+ memory use; a routing proxy (claude-code-router-style) cannot route Claude subscriptions compliantly. Rejected for M1; the event-stream core makes a daemon an additive frontend later.

````

with:

````markdown
  harness-sandbox    macOS Seatbelt; Linux Landlock + seccomp
  harness-context    AGENTS.md/CLAUDE.md discovery; prompt assembly; slash-command discovery and expansion
  harness-config     XDG paths, config layering, workspace trust
  harness-tui        inline terminal UI: rendering, input, approvals, the interactive session's loop
  harness-cli        `harness` binary: clap subcommands, starting the agent for `ask` and the TUI, NDJSON output
```

The core runs on `tokio` and emits `AgentEvent`s: `TurnStarted`, `TextDelta`, `ReasoningDelta`, `ToolCallRequested`, `ApprovalNeeded`, `ToolCallFinished`, `Usage`, `TurnStats`, `Steered`, `Retrying`, `Compacted`, `CheckpointCreated`, `TurnFinished { reason }`, `Error { kind }`. Every assistant message carries the id of the model that produced it. Frontends send back user input (queued input the frontend holds until the turn ends; send-now input through a `Steering` handle the agent reads at each tool-result boundary), `ApprovalDecision`, and `Interrupt` (cancelling the turn's token). The TUI, `ask` (plain), and `ask --json` (NDJSON) are consumers; the M4 ACP server will be another.

*Alternatives:* a local daemon plus clients (OpenCode-style) enables shared live sessions but adds a daemon, ports, and auth, and OpenCode draws criticism for 1 GB+ memory use; a routing proxy (claude-code-router-style) cannot route Claude subscriptions compliantly. Rejected for M1; the event-stream core makes a daemon an additive frontend later.

````

Replace (2 of 4):

```markdown
    - changes directory again so its working directory resolves inside the new mounts.
    
    In both tiers the seccomp filter also refuses the mount and namespace syscalls (`mount`, `umount2`, `mount_setattr`, `move_mount`, `open_tree`, `open_tree_attr`, `fsopen`, `fsconfig`, `fsmount`, `fspick`, `pivot_root`, `unshare`, `setns`, and `clone` with any `CLONE_NEW*` flag), and makes `clone3`, whose flags it cannot read, fail with `ENOSYS`, so runtimes fall back to `clone`. Mounts copied into any nested namespace are locked. A workspace with nothing to protect skips the namespace. If any step fails the command does not run. When the kernel or host refuses a step (the namespace, the id maps, or a mount call itself), the session drops to the basic tier with a warning; when a step fails because a path changed in the meantime, or a resource limit was hit, only that command is blocked and the session keeps the full tier, so something outside a command cannot downgrade the session.
  - **Basic tier.** Landlock, seccomp and the guard, with one startup warning naming `harness sandbox doctor`. That command reports the tier, why it was chosen, and the exact fix (an AppArmor profile for the harness binary, or the sysctl); it never changes system files itself. Setting `sandbox.linux_git_protection = "required"` makes the basic tier ask before every shell command in `ask` and `auto` (`plan` and `read-only` keep their read-only sandbox, which already protects git metadata); the default, `"best-effort"`, keeps `auto` usable.
  - **The guard, in both tiers.** Mounts cannot cover a name that does not exist yet, and there is no safe placeholder for `commondir` (git refuses an empty one). The guard runs in the harness process around each sandboxed command. While the command runs, an inotify watcher on each known gitdir and on the workspace root catches a new `commondir`, `config`, `config.worktree`, `hooks`, `gitweb` or `pid` in a gitdir, a top-level `HEAD` and `.harness/`. These are the places where a git process outside the sandbox, such as an editor's background status poll, would pick up a planted file at once. In both tiers the watcher also catches changes to the existing protected files and gitfiles, which the guard snapshots before the command: in the full tier too, because renaming or removing a file from outside a mount namespace, as git does whenever it rewrites `.git/config`, detaches the read-only bind on it in every other namespace, so a command running while the user runs git could otherwise write the new file; in both tiers the guard snapshots what a mount cannot protect: a protected symlink, and a protected file with a second hard link. After the command, a final check repeats the gitdir checks, and a scan of the workspace (skipping git-ignored directories) then finds any new nested `.git`. Before the next command the guard checks again for new protected names in the gitdirs it knew and at the top of the workspace, to catch anything a background process left behind; a repository created between commands, which may be the user's own clone, is left alone.
    - The scan reads the workspace's ignore rules (`.gitignore` files and each repository's `info/exclude`) once, when the session starts, and uses them for every scan. So a command cannot hide a new repository behind an ignore rule it wrote, and neither can the model's file tools. The cost: a rule you add mid-session takes effect after a restart. Ignore files are read only if they are regular files, within size and complexity limits. Each scan stops after a fixed number of entries or 5 seconds. A scan that could not cover the whole workspace is reported once per session. If a command makes a fully scanned workspace impossible to scan fully, that command counts as blocked. A `.git` or nested gitdir absent before the command counts as new when the scan before the command was complete, since that scan saw everything; otherwise it is reported and left in place.
    - **Background processes.** A command can leave a process running that changes a protected file after the command ends. In the full tier such a process stays in the command's mount namespace, so the entries mounted read-only there stay protected, but a protected entry that appears later (a worktree or submodule gitdir added afterwards) is not mounted in that namespace. So in both tiers harness tracks the processes each command leaves running, including detached ones: it registers as a child subreaper, so orphans reparent to it. While any of them lives, the watcher keeps checking after the command ends, and the check before the next command also restores changed protected files and gitfiles from the snapshot. Restoring also undoes a change you make to those files while such a process runs. The changed version is kept in the quarantine and reported. When no such process is left, changes between commands are yours and are left alone. When harness exits, it ends the processes sandboxed commands left running (`SIGTERM`, then `SIGKILL` after about a second), runs one last check that undoes any change they made to protected files, and prints what it did to stderr without changing the exit code; processes from commands you approved to run outside the sandbox are left alone.
```

with:

```markdown
    - changes directory again so its working directory resolves inside the new mounts.
    
    In both tiers the seccomp filter also refuses the mount and namespace syscalls (`mount`, `umount2`, `mount_setattr`, `move_mount`, `open_tree`, `open_tree_attr`, `fsopen`, `fsconfig`, `fsmount`, `fspick`, `pivot_root`, `unshare`, `setns`, and `clone` with any `CLONE_NEW*` flag), and makes `clone3`, whose flags it cannot read, fail with `ENOSYS`, so runtimes fall back to `clone`. Mounts copied into any nested namespace are locked. A workspace with nothing to protect skips the namespace. If any step fails the command does not run. When the kernel or host refuses a step (the namespace, the id maps, or a mount call itself), the session drops to the basic tier with a warning; when a step fails because a path changed in the meantime, or a resource limit was hit, only that command is blocked and the session keeps the full tier, so something outside a command cannot downgrade the session.
  - **Basic tier.** Landlock, seccomp and the guard, with one startup warning naming `harness sandbox doctor`. That command reports the tier, why it was chosen, and the exact fix (an AppArmor profile for the harness binary, or the sysctl); it never changes system files itself. Setting `sandbox.linux_git_protection = "required"` makes the basic tier ask before every shell command in `ask` and `auto` (`plan` and `read-only` keep their read-only sandbox, which already protects git metadata); the default, `"best-effort"`, keeps `auto` usable. A session that drops to the basic tier after starting in the full one asks too: the sandbox then says it cannot run a command that may write (`CommandSandbox::cannot_run`), and the agent asks whether to run that command outside the sandbox, once; a headless run counts it as blocked (exit 3). Both tiers save every protected file before each command.
  - **The guard, in both tiers.** Mounts cannot cover a name that does not exist yet, and there is no safe placeholder for `commondir` (git refuses an empty one). The guard runs in the harness process around each sandboxed command. While the command runs, an inotify watcher on each known gitdir and on the workspace root catches a new `commondir`, `config`, `config.worktree`, `hooks`, `gitweb` or `pid` in a gitdir, a top-level `HEAD` and `.harness/`. These are the places where a git process outside the sandbox, such as an editor's background status poll, would pick up a planted file at once. In both tiers the watcher also catches changes to the existing protected files and gitfiles, which the guard snapshots before the command: in the full tier too, because renaming or removing a file from outside a mount namespace, as git does whenever it rewrites `.git/config`, detaches the read-only bind on it in every other namespace, so a command running while the user runs git could otherwise write the new file; in both tiers the guard snapshots what a mount cannot protect: a protected symlink, and a protected file with a second hard link. After the command, a final check repeats the gitdir checks, and a scan of the workspace (skipping git-ignored directories) then finds any new nested `.git`. Before the next command the guard checks again for new protected names in the gitdirs it knew and at the top of the workspace, to catch anything a background process left behind; a repository created between commands, which may be the user's own clone, is left alone.
    - The scan reads the workspace's ignore rules (`.gitignore` files and each repository's `info/exclude`) once, when the session starts, and uses them for every scan. So a command cannot hide a new repository behind an ignore rule it wrote, and neither can the model's file tools. The cost: a rule you add mid-session takes effect after a restart. Ignore files are read only if they are regular files, within size and complexity limits. Each scan stops after a fixed number of entries or 5 seconds. A scan that could not cover the whole workspace is reported once per session. If a command makes a fully scanned workspace impossible to scan fully, that command counts as blocked. A `.git` or nested gitdir absent before the command counts as new when the scan before the command was complete, since that scan saw everything; otherwise it is reported and left in place.
    - **Background processes.** A command can leave a process running that changes a protected file after the command ends. In the full tier such a process stays in the command's mount namespace, so the entries mounted read-only there stay protected, but a protected entry that appears later (a worktree or submodule gitdir added afterwards) is not mounted in that namespace. So in both tiers harness tracks the processes each command leaves running, including detached ones: it registers as a child subreaper, so orphans reparent to it. While any of them lives, the watcher keeps checking after the command ends, and the check before the next command also restores changed protected files and gitfiles from the snapshot. Restoring also undoes a change you make to those files while such a process runs. The changed version is kept in the quarantine and reported. When no such process is left, changes between commands are yours and are left alone. When harness exits, it ends the processes sandboxed commands left running (`SIGTERM`, then `SIGKILL` after about a second), runs one last check that undoes any change they made to protected files, and prints what it did to stderr without changing the exit code; processes from commands you approved to run outside the sandbox are left alone.
```

Replace (3 of 4):

```markdown

### D7. Sessions

Append-only JSONL at `$XDG_DATA_HOME/harness/sessions/<project-key>/<session-id>.jsonl`, where `<project-key>` is derived from the canonical repo root path (or the working directory outside a repo): its last component and 16 hex digits of the path's SHA-256. Each entry has `id` and `parent_id`; the session's active branch is the path from the root to the most recent leaf. The first line is the session's own entry, whose `id` is the session id, and every other entry descends from it. Session ids are 1 to 64 ASCII letters, digits and dashes (the start time and 32 random bits, such as `20260927T123456Z-1a2b3c4d`), since they become parts of paths; a file is a session only when it is a regular file named after the id its first line gives. The first line also gives the format version the file was started in; a harness that continues a file in an older format first appends a `version` entry recording its own (off the active branch, in the same write as the entry that follows it), and a file whose first line or any `version` entry names a newer format than the running harness understands is refused before anything in it changes. Older files still load. Session files hold prompts, code and tool output, so they are readable only by their owner (files 0600, folders 0700). `/rewind` moves the active leaf, so rewinding creates a branch without deleting history; the move is itself an entry, so it survives a restart. The file is created with the first message and locked while a process uses it, so a second process refuses it; on a file system that cannot lock files, it is used unlocked with a warning. Loading skips unreadable lines with a warning, stops at a missing parent or a loop in the entries with a warning, and resuming removes an incomplete last line before appending. Providers reject a tool call without a result, so a call that a killed run left without one gets one when the session is loaded, saying harness stopped before it finished and its effects are unknown. `-c` continues the project's most recently used session; until the terminal UI's picker, `--resume` without an id (and without a subcommand) lists the sessions (id, start time, first message) and `--resume <id>` continues one. Only `harness ask` continues a session, so `-c` or `--resume <id>` with another subcommand is refused with a message saying to run it without the flag, and `harness ask --resume "fix it"`, where the prompt was taken for the id, says `--resume` needs an id, followed by the prompt.

Auto-compaction triggers when the next request would reach 80% of the model's context window (configurable), and keeps the most recent turns verbatim within a budget (default 20% of the window); the summary is displayed to the user, stored as a `compaction` entry, and the originals stay on disk so the user can rewind to before the compaction. The settings are `[compaction] threshold_percent` and `keep_recent_percent`. The size of the next request is the input tokens the provider reported for the last one plus estimates for the messages since, the reply included, or else an estimate of the whole request; output tokens, reasoning included, are not sent back, so they do not count. The model answering the turn writes the summary (the session's model, or the model a slash command chose for that turn), in a request of its own, from a short summarizing prompt and a transcript of the older messages, each clipped (an earlier summary only to the whole transcript), with the oldest dropped to fit half the window; a summary request the model rejects as too long is sent once more with half that, and a summary cut off at the model's output limit is rejected. When no recent part fits the budget, automatic compaction keeps the current turn, or only its last step (the last model reply with its tool results) when the turn itself reaches the threshold; overflow compaction keeps the last step; and `/compact` summarizes everything. Summarizing nothing but an earlier summary counts as nothing to compact, and once an automatic compaction leaves the request at or over the threshold, automatic compaction waits until it drops below, so it never repeats without shrinking anything. A context overflow is recognised from the error text of an HTTP 400, 413 or 422 response, or of an error the provider reports inside its stream; never from a response harness could not parse, which quotes what it received and can hold the model's own text. Until model profiles report the real window, harness assumes 32,768 tokens.

### D8. Inline terminal UI

ratatui inline viewport (as in Codex): a live region at the bottom (input, streaming output, approval prompts), with finished messages inserted into normal terminal scrollback; only the live region is redrawn, so tmux and scrollback stay intact. Markdown via `pulldown-cmark`, code highlighting via `syntect`, diffs via `similar`, fuzzy file completion via `nucleo`. Pickers, `/rewind`, and long diffs use a temporary full-screen view and return to inline.

- **Steering:** input submitted while a turn runs is queued and sent when the turn ends; the send-now key (default Ctrl+S; raw mode disables XON/XOFF) delivers it at the next tool boundary instead.
- **Notifications:** when a turn that ran longer than 10 seconds finishes, or an approval is needed, harness emits an OSC 9 desktop notification and a terminal bell (configurable).
- **Pastes** over 10 lines or 1,000 characters collapse to a `[Pasted text #n, N lines]` placeholder that can be expanded for editing; the full text is sent.
- **Stats:** after each turn, a dim line shows the model, time to first token, output tokens per second, and prompt-cache hit rate when reported.
- `NO_COLOR` is honoured; no ANSI output when stdout is not a TTY.

### D9. Configuration and workspace trust

Paths follow the XDG base-directory spec on macOS and Linux: config `$XDG_CONFIG_HOME/harness` (default `~/.config/harness`), data `$XDG_DATA_HOME/harness` (default `~/.local/share/harness`: sessions, checkpoints, trust list, credential fallback), state `$XDG_STATE_HOME/harness` (default `~/.local/state/harness`: logs, tool output). `HARNESS_HOME`, when set, overrides all three with subdirectories of one path.

TOML layering: global `config.toml` ← project `.harness/config.toml` ← CLI flags. Project settings that could widen the harness's reach (a `mode` that grants more than the effective global mode, a `max_steps` above the effective global limit, `model`, `[permissions].allow`, `read_dirs`, provider definitions or `base_url` overrides, and any `[sandbox]` setting except `allow_localhost = false`) are applied only after the user trusts the workspace. The effective global mode is the global config's `mode`, else the default for the workspace (`auto` in a git work tree, `ask` elsewhere), and modes rank `plan` = `read-only` < `ask` < `auto` < `full-access`; the effective global limit is the global `max_steps`, else 50. Trust is stored in the data directory as a fingerprint (SHA-256) of those settings; if they change, the workspace is untrusted again. A workspace with none can be trusted too (the fingerprint of the empty set), so that its project command files may choose their model. `harness trust` shows the settings and records trust; the interactive first-use prompt arrives with the terminal UI. Headless runs ignore untrusted widening settings with a warning. Narrowing settings (`deny`, `confirm`, a mode or `max_steps` no wider than the effective global one, and `allow_localhost = false`) always apply. `[compaction]` settings change when harness summarizes, not what the agent may do, so a project sets them without trust, except a `threshold_percent` below 50: summarizing every few turns costs paid requests and verbatim context, so such a threshold is shown and fingerprinted with the widening settings, and until trusted the global value or the default applies, with a warning.

Example global config:

```

with:

```markdown

### D7. Sessions

Append-only JSONL at `$XDG_DATA_HOME/harness/sessions/<project-key>/<session-id>.jsonl`, where `<project-key>` is derived from the canonical repo root path (or the working directory outside a repo): its last component and 16 hex digits of the path's SHA-256. Each entry has `id` and `parent_id`; the session's active branch is the path from the root to the most recent leaf. The first line is the session's own entry, whose `id` is the session id, and every other entry descends from it. Session ids are 1 to 64 ASCII letters, digits and dashes (the start time and 32 random bits, such as `20260927T123456Z-1a2b3c4d`), since they become parts of paths; a file is a session only when it is a regular file named after the id its first line gives. The first line also gives the format version the file was started in; a harness that continues a file in an older format first appends a `version` entry recording its own (off the active branch, in the same write as the entry that follows it), and a file whose first line or any `version` entry names a newer format than the running harness understands is refused before anything in it changes. Older files still load. Session files hold prompts, code and tool output, so they are readable only by their owner (files 0600, folders 0700). `/rewind` moves the active leaf, so rewinding creates a branch without deleting history; the move is itself an entry, so it survives a restart. The file is created with the first message and locked while a process uses it, so a second process refuses it; on a file system that cannot lock files, it is used unlocked with a warning. Loading skips unreadable lines with a warning, stops at a missing parent or a loop in the entries with a warning, and resuming removes an incomplete last line before appending. Providers reject a tool call without a result, so a call that a killed run left without one gets one when the session is loaded, saying harness stopped before it finished and its effects are unknown. `-c` continues the project's most recently used session; until the terminal UI's picker, `--resume` without an id (and without a subcommand) lists the sessions (id, start time, first message) and `--resume <id>` continues one. Only `harness ask` and the interactive session (`harness` without a subcommand) continue a session, so `-c` or `--resume <id>` with another subcommand is refused with a message saying to run it without the flag, and `harness ask --resume "fix it"`, where the prompt was taken for the id, says `--resume` needs an id, followed by the prompt.

Auto-compaction triggers when the next request would reach 80% of the model's context window (configurable), and keeps the most recent turns verbatim within a budget (default 20% of the window); the summary is displayed to the user, stored as a `compaction` entry, and the originals stay on disk so the user can rewind to before the compaction. The settings are `[compaction] threshold_percent` and `keep_recent_percent`. The size of the next request is the input tokens the provider reported for the last one plus estimates for the messages since, the reply included, or else an estimate of the whole request; output tokens, reasoning included, are not sent back, so they do not count. The model answering the turn writes the summary (the session's model, or the model a slash command chose for that turn), in a request of its own, from a short summarizing prompt and a transcript of the older messages, each clipped (an earlier summary only to the whole transcript), with the oldest dropped to fit half the window; a summary request the model rejects as too long is sent once more with half that, and a summary cut off at the model's output limit is rejected. When no recent part fits the budget, automatic compaction keeps the current turn, or only its last step (the last model reply with its tool results) when the turn itself reaches the threshold; overflow compaction keeps the last step; and `/compact` summarizes everything. Summarizing nothing but an earlier summary counts as nothing to compact, and once an automatic compaction leaves the request at or over the threshold, automatic compaction waits until it drops below, so it never repeats without shrinking anything. A context overflow is recognised from the error text of an HTTP 400, 413 or 422 response, or of an error the provider reports inside its stream; never from a response harness could not parse, which quotes what it received and can hold the model's own text. Until model profiles report the real window, harness assumes 32,768 tokens.

### D8. Inline terminal UI

An inline terminal (as in Codex): a live region at the bottom (input, streaming output, approval prompts), with finished messages inserted into normal terminal scrollback; only the live region is redrawn, and only its cells that changed, so tmux and scrollback stay intact. The `harness-tui` crate draws with ratatui's backend and buffers, but keeps its own inline terminal rather than ratatui's `Viewport::Inline`, whose height is fixed: the live region grows and shrinks with what it shows, and finished lines scroll into scrollback by scrolling the screen. The agent runs in a task of its own, fed turns through a channel, while the UI loop reads keys and the agent's events; tests drive it with scripted keys and pastes on ratatui's `TestBackend` against the mock provider. Markdown via `pulldown-cmark`, code highlighting via `syntect` (its bundled syntaxes and a dark theme, 24-bit colour when `COLORTERM` says so, else the nearest of 256), diffs via `similar`, fuzzy file completion via `nucleo-matcher` (the matcher of `nucleo`, without its thread pool). Pickers and `/rewind` use a temporary full-screen view and return to inline (with the rest of the UI after P4); a long diff scrolls inside the approval prompt. Interactive mode needs a terminal on stdin and stdout; without one, `harness` exits 2 and names `harness ask`.

- **Keys:** Enter sends; Alt+Enter, Shift+Enter (where the terminal reports it), Ctrl+J, or a `\` before Enter insert a new line; Up and Down move between rows, then recall earlier inputs (this session's user messages, and a resumed session's); Tab completes a `/` command or `@` path; Ctrl+O expands a collapsed paste; Esc interrupts a turn (or closes the completion list); Ctrl+C interrupts a turn or clears the input, and a second Ctrl+C within 2 seconds exits, as does Ctrl+D on empty input; Shift+Tab cycles `plan`, `ask` and `auto`. Raw mode turns off XON/XOFF, so Ctrl+S arrives; disambiguated keys (the kitty protocol) are asked for where supported.
- **Steering:** input submitted with Enter while a turn runs is queued and sent as a new turn when the turn ends; with the send-now key (Ctrl+S) it is given to the model with the results of the running turn's next tool calls (`Steered` event). Send-now input no tool result took before the turn ended starts the next turn, before queued input. After an interruption, queued and unsent send-now input go back into the editor instead. Built-in commands that need no turn (`/help`, `/context`, `/usage`) run at once; a slash command cannot be sent now, only queued.
- **Approvals:** the prompt shows the reason, the command or, for `write` and `edit`, a unified diff against the file as it is (files over 1 MB or not text get a note instead; only the user sees it); `y` approves once, `a` for the session, `n` denies with an optional reason for the model, Esc (or Ctrl+C) denies and stops the turn. The offer to run a command outside the sandbox has no session choice. A session approval the policy cannot keep (destructive commands, and others it always asks about) says it applied once.
- **Mode switching:** Shift+Tab switches between turns; during a turn it picks the mode to switch to when the turn ends. From `read-only` it goes to `ask`, from `full-access` to `plan`. The agent takes a sandbox per access (`Sandboxes`): `plan` and `read-only` keep a read-only sandbox where `ask` and `auto` have none (a too-broad workspace, the Linux basic tier with `"required"`), and `full-access` has none, so a switch also changes what shell commands run in. The interactive session looks for a sandbox whatever mode it starts in.
- **Notifications:** when a turn that ran 10 seconds or longer ends (not by the user's interruption), or an approval waits, harness writes an OSC 9 desktop notification (`ESC ] 9 ; harness: <text> BEL`, the text stripped of control characters) and a bell. `[notifications] desktop` and `bell` turn each off; a project may set them without trust.
- **Pastes** over 10 lines or 1,000 characters collapse to a `[Pasted text #n, N lines]` placeholder that deletes as a unit and expands with Ctrl+O; the full text is sent, and a recalled long input comes back collapsed.
- **Stats and the status line:** each turn ends with a `TurnStats` event (the model that answered last, time to first token, streaming time, and the tokens the provider reported), which `ask --json` prints too; the UI shows it as a dim line: the model, time to first token, output tokens per second, and the prompt-cache hit rate when the provider reported cached tokens. The status line shows the model, the mode, the next request's share of the context window, and the session's input and output tokens; in `full-access`, a warning. `/context` splits the window into the system prompt, tool definitions, each instruction file, the conversation and free space; `/usage` shows input, output and cached tokens per model since harness started. The window is the assumed 32,768 tokens until model profiles (P4) report the model's own.
- `NO_COLOR` is honoured; no ANSI output when stdout is not a TTY.

### D9. Configuration and workspace trust

Paths follow the XDG base-directory spec on macOS and Linux: config `$XDG_CONFIG_HOME/harness` (default `~/.config/harness`), data `$XDG_DATA_HOME/harness` (default `~/.local/share/harness`: sessions, checkpoints, trust list, credential fallback), state `$XDG_STATE_HOME/harness` (default `~/.local/state/harness`: logs, tool output). `HARNESS_HOME`, when set, overrides all three with subdirectories of one path.

TOML layering: global `config.toml` ← project `.harness/config.toml` ← CLI flags. Project settings that could widen the harness's reach (a `mode` that grants more than the effective global mode, a `max_steps` above the effective global limit, `model`, `[permissions].allow`, `read_dirs`, provider definitions or `base_url` overrides, and any `[sandbox]` setting except `allow_localhost = false`) are applied only after the user trusts the workspace. The effective global mode is the global config's `mode`, else the default for the workspace (`auto` in a git work tree, `ask` elsewhere), and modes rank `plan` = `read-only` < `ask` < `auto` < `full-access`; the effective global limit is the global `max_steps`, else 50. Trust is stored in the data directory as a fingerprint (SHA-256) of those settings; if they change, the workspace is untrusted again. A workspace with none can be trusted too (the fingerprint of the empty set), so that its project command files may choose their model. `harness trust` shows the settings and records trust. At the start of an interactive session, a workspace whose widening settings are not trusted gets them listed and one question; yes records trust as `harness trust` does, so they apply at once, and no keeps them off and asks again next time. Headless runs ignore untrusted widening settings with a warning. Narrowing settings (`deny`, `confirm`, a mode or `max_steps` no wider than the effective global one, and `allow_localhost = false`) always apply. `[compaction]` settings change when harness summarizes, not what the agent may do, so a project sets them without trust, except a `threshold_percent` below 50: summarizing every few turns costs paid requests and verbatim context, so such a threshold is shown and fingerprinted with the widening settings, and until trusted the global value or the default applies, with a warning.

Example global config:

```

Replace (4 of 4):

```markdown

### D12. Plan mode

`plan` is an approval mode with read-only permissions plus a short planning instruction appended to the conversation (not the system prompt, see D15). The turn ends with a plan message. The user can then choose **Build** (switch back to the previous non-plan mode and send "Implement the plan above"), **Edit** (open the plan in `$EDITOR`; the edited text is presented again for approval), or **Keep planning**. The approved plan is stored as a `plan` session entry. M2 attaches a different model to the build step (planner/builder split).

### D13. Checkpoints and rewind

```

with:

```markdown

### D12. Plan mode

`plan` is an approval mode with read-only permissions plus a short planning instruction appended to the conversation (not the system prompt, see D15): the note that records the switch to `plan` asks the model to investigate, then end its reply with a step-by-step implementation plan. An interactive session that starts in `plan` gives that note with its first message. The turn ends with a plan message. The user can then choose **Build** (switch back to the mode before `plan`, or, for a session that started in `plan`, the configured mode unless it is `plan` or `read-only`, else the workspace's default; then send "Implement the plan above.", or the edited plan itself after an edit), **Edit** (open the plan in `$EDITOR`, run as `sh -c '$EDITOR "$1"'` and falling back to `vi`, with the terminal's modes undone meanwhile; the edited text is presented again for approval), or **Keep planning**. The approved plan is stored as a `plan` field on the Build turn's user message entry, which older harness versions ignore, so no format change is needed. A headless `--mode plan` run stays read-only, without the planning instruction or the choices. M2 attaches a different model to the build step (planner/builder split).

### D13. Checkpoints and rewind

```

- [ ] **Step 2: Update the specs**

In `openspec/changes/add-core-agent/specs/cli-interface/spec.md`:

Replace (1 of 3):

```markdown
## ADDED Requirements

### Requirement: Interactive sessions render inline
Interactive mode SHALL render into the terminal's normal screen: completed messages MUST be written into the terminal scrollback, and only the active region (input, streaming output, prompts) MUST be redrawn. Full-screen views MAY be used for pickers, `/rewind`, and long diffs and MUST return to inline mode when closed.

#### Scenario: Scrollback preserved
- **WHEN** a session produces more output than fits on screen
- **THEN** earlier messages remain reachable with the terminal's own scrollback

### Requirement: Status line and per-turn stats
Interactive mode SHALL display a status line showing the active model, approval mode, context usage as a percentage of the effective context window, and session token totals. After each turn it MUST show the model that answered, time to first token, output tokens per second, and prompt-cache hit rate when the provider reports cache usage.

#### Scenario: Status after switching model
- **WHEN** the user switches to a model with a larger context window
```

with:

```markdown
## ADDED Requirements

### Requirement: Interactive sessions render inline
Interactive mode SHALL render into the terminal's normal screen: completed messages MUST be written into the terminal scrollback, and only the active region (input, streaming output, prompts) MUST be redrawn. Full-screen views MAY be used for pickers, `/rewind`, and long diffs and MUST return to inline mode when closed. Interactive mode MUST need a terminal on standard input and standard output; without one, `harness` MUST exit with code 2 and name `harness ask`.

#### Scenario: Scrollback preserved
- **WHEN** a session produces more output than fits on screen
- **THEN** earlier messages remain reachable with the terminal's own scrollback

#### Scenario: No terminal
- **WHEN** the user runs `harness` with standard input from a pipe
- **THEN** harness exits with code 2 and says to use `harness ask`

### Requirement: Status line and per-turn stats
Interactive mode SHALL display a status line showing the active model, approval mode, context usage as a percentage of the effective context window, and session token totals. After each turn it MUST show the model that answered, time to first token, output tokens per second, and prompt-cache hit rate when the provider reports cached tokens. The runtime MUST report these per-turn statistics as an event before the turn finishes, so `harness ask --json` prints them too.

#### Scenario: Status after switching model
- **WHEN** the user switches to a model with a larger context window
```

Replace (2 of 3):

```markdown
- **THEN** a stats line shows the model, time to first token, tokens per second, and cache hit rate

### Requirement: Keyboard interaction
Interactive mode SHALL support: Esc to interrupt the running turn; Esc twice on empty input to open `/rewind`; Ctrl+C pressed twice within 2 seconds to exit; Shift+Tab to cycle approval modes; Alt+Enter or Shift+Enter to insert a newline; Up arrow to recall previous inputs; `/` at the start of input for command completion; `@` for fuzzy completion of workspace file paths; Enter while a turn is running to queue input; and Ctrl+S while a turn is running to send input immediately (steering).

#### Scenario: File completion
- **WHEN** the user types `@mainrs`
```

with:

```markdown
- **THEN** a stats line shows the model, time to first token, tokens per second, and cache hit rate

### Requirement: Keyboard interaction
Interactive mode SHALL support: Esc to interrupt the running turn; Esc twice on empty input to open `/rewind`; Ctrl+C pressed twice within 2 seconds to exit; Shift+Tab to cycle approval modes; Alt+Enter or Shift+Enter to insert a newline, with Ctrl+J and a backslash before Enter as fallbacks for terminals that do not report Shift+Enter; Up arrow to recall previous inputs; `/` at the start of input for command completion; `@` for fuzzy completion of workspace file paths; Enter while a turn is running to queue input; and Ctrl+S while a turn is running to send input immediately (steering). An approval prompt MUST take y (approve once), a (approve for the session, when offered), n (deny, with an optional reason for the model) and Esc (deny and stop the turn).

#### Scenario: File completion
- **WHEN** the user types `@mainrs`
```

Replace (3 of 3):

```markdown
- **THEN** the message is delivered to the model at the next tool-result boundary

### Requirement: Desktop notifications
Interactive mode SHALL emit an OSC 9 desktop notification and a terminal bell when a turn that ran longer than 10 seconds finishes, or when an approval is needed. Both MUST be configurable and MUST be disabled when stdout is not a terminal.

#### Scenario: Long task completes
- **WHEN** a turn runs for 3 minutes and finishes
- **THEN** the terminal receives an OSC 9 notification and a bell

### Requirement: Large pastes are collapsed
Interactive mode SHALL display pasted text longer than 10 lines or 1,000 characters as a numbered placeholder showing its line count, let the user expand the placeholder to edit the text, and send the full text with the message.

#### Scenario: Pasting a stack trace
- **WHEN** the user pastes a 200-line stack trace
```

with:

```markdown
- **THEN** the message is delivered to the model at the next tool-result boundary

### Requirement: Desktop notifications
Interactive mode SHALL emit an OSC 9 desktop notification and a terminal bell when a turn that ran 10 seconds or longer finishes, other than by the user's interruption, or when an approval is needed. Both MUST be configurable (`[notifications] desktop` and `bell`, on by default) and MUST be disabled when stdout is not a terminal. Text in a notification MUST have its control characters removed.

#### Scenario: Long task completes
- **WHEN** a turn runs for 3 minutes and finishes
- **THEN** the terminal receives an OSC 9 notification and a bell

#### Scenario: Notifications turned off
- **WHEN** the configuration sets `[notifications] desktop = false`
- **THEN** a long turn ends with a bell and no OSC 9 notification

### Requirement: Large pastes are collapsed
Interactive mode SHALL display pasted text longer than 10 lines or 1,000 characters as a numbered placeholder showing its line count, let the user expand the placeholder to edit the text (Ctrl+O), and send the full text with the message.

#### Scenario: Pasting a stack trace
- **WHEN** the user pastes a 200-line stack trace
```

In `openspec/changes/add-core-agent/specs/agent-runtime/spec.md`:

Replace (1 of 2):

```markdown
- **THEN** the runtime stops calling the model and finishes the turn with reason `step_limit`

### Requirement: User input during a turn is queued or steered
The runtime SHALL accept user input while a turn is running. Input marked as queued MUST be delivered as the next user message after the turn finishes. Input marked as send-now MUST be delivered to the model at the next tool-result boundary within the running turn.

#### Scenario: Queued message
- **WHEN** the user submits "also update the README" as queued input while the agent is running tests
```

with:

```markdown
- **THEN** the runtime stops calling the model and finishes the turn with reason `step_limit`

### Requirement: User input during a turn is queued or steered
The runtime SHALL accept user input while a turn is running. Input marked as queued MUST be delivered as the next user message after the turn finishes. Input marked as send-now MUST be delivered to the model at the next tool-result boundary within the running turn, and reported as a steered event; send-now input that no tool-result boundary took before the turn ended MUST be delivered as the next user message, before queued input. When the turn is interrupted, input not yet delivered MUST NOT be sent: the interactive frontend returns it to the input editor.

#### Scenario: Queued message
- **WHEN** the user submits "also update the README" as queued input while the agent is running tests
```

Replace (2 of 2):

```markdown
#### Scenario: Steering mid-turn
- **WHEN** the user submits "use the v2 API instead" as send-now input while a tool call is running
- **THEN** the message is included with the next tool result sent to the model in the same turn

### Requirement: Turns can be interrupted
The runtime SHALL accept an interrupt at any point in a turn. On interrupt it MUST cancel the in-flight model request, terminate any running tool process including its child processes, keep the partial output already received, and finish the turn with reason `interrupted`.
```

with:

```markdown
#### Scenario: Steering mid-turn
- **WHEN** the user submits "use the v2 API instead" as send-now input while a tool call is running
- **THEN** the message is included with the next tool result sent to the model in the same turn

#### Scenario: Send-now input after the last tool call
- **WHEN** the user submits send-now input while the model writes a final answer that calls no tools
- **THEN** the input is sent as the next user turn once that turn finishes

### Requirement: Turns can be interrupted
The runtime SHALL accept an interrupt at any point in a turn. On interrupt it MUST cancel the in-flight model request, terminate any running tool process including its child processes, keep the partial output already received, and finish the turn with reason `interrupted`.
```

In `openspec/changes/add-core-agent/specs/plan-mode/spec.md`:

Replace (1 of 3):

```markdown
## ADDED Requirements

### Requirement: Plan mode is read-only exploration ending in a plan
In `plan` mode the system SHALL apply read-only permissions, instruct the model to investigate and finish with a step-by-step implementation plan, and present the plan with the choices Build, Edit, and Keep planning. The planning instruction MUST be appended to the conversation rather than changing the system prompt.

#### Scenario: Planning a feature
- **WHEN** the user enters `plan` mode and asks for a login rate limiter
```

with:

```markdown
## ADDED Requirements

### Requirement: Plan mode is read-only exploration ending in a plan
In `plan` mode the system SHALL apply read-only permissions, instruct the model to investigate and finish with a step-by-step implementation plan, and, in interactive mode, present the plan with the choices Build, Edit, and Keep planning. The planning instruction MUST be appended to the conversation rather than changing the system prompt: with the note that records the switch to `plan`, or with the first message of an interactive session that starts in `plan`.

#### Scenario: Planning a feature
- **WHEN** the user enters `plan` mode and asks for a login rate limiter
```

Replace (2 of 3):

```markdown
- **AND** the turn ends with a plan and the three choices

### Requirement: Approving a plan starts implementation
Choosing Build SHALL store the plan as the approved plan in the session, switch to the mode that was active before `plan` mode (or the default mode if there was none), and start a turn instructing the model to implement the approved plan.

#### Scenario: Build from plan
- **WHEN** the user was in `auto` mode, switched to `plan`, and chooses Build
```

with:

```markdown
- **AND** the turn ends with a plan and the three choices

### Requirement: Approving a plan starts implementation
Choosing Build SHALL store the plan as the approved plan in the session (with the Build turn's user message), switch to the mode that was active before `plan` mode (or, if there was none, the configured mode unless it is `plan` or `read-only`, else the workspace's default mode), and start a turn instructing the model to implement the approved plan; after an edit, that turn MUST carry the edited plan itself.

#### Scenario: Build from plan
- **WHEN** the user was in `auto` mode, switched to `plan`, and chooses Build
```

Replace (3 of 3):

```markdown
- **THEN** the plan shown for approval no longer contains step 3

### Requirement: Mode cycling
In interactive mode, Shift+Tab SHALL cycle the approval mode through `plan`, `ask`, and `auto`, and MUST never enter `full-access`. `/mode <name>` and `--mode <name>` MUST also select `plan`.

#### Scenario: Cycling modes
- **WHEN** the session is in `auto` mode and the user presses Shift+Tab
- **THEN** the mode becomes `plan` and the status line shows it
```

with:

```markdown
- **THEN** the plan shown for approval no longer contains step 3

### Requirement: Mode cycling
In interactive mode, Shift+Tab SHALL cycle the approval mode through `plan`, `ask`, and `auto`, and MUST never enter `full-access` or `read-only`; from `read-only` it goes to `ask`, and from `full-access` to `plan`. A switch requested while a turn runs MUST take effect when the turn ends. `/mode <name>` and `--mode <name>` MUST also select `plan`.

#### Scenario: Cycling modes
- **WHEN** the session is in `auto` mode and the user presses Shift+Tab
- **THEN** the mode becomes `plan` and the status line shows it

#### Scenario: Cycling during a turn
- **WHEN** the user presses Shift+Tab while a turn runs in `auto` mode
- **THEN** the status line shows that `plan` applies after the turn, and the mode becomes `plan` when the turn ends
```

In `openspec/changes/add-core-agent/specs/permissions-sandbox/spec.md`:

Replace (1 of 4):

```markdown
- **THEN** the user is asked to approve before the file is read

### Requirement: Approval prompts offer once, session, or deny with feedback
When approval is required interactively, the system SHALL offer: approve once; approve for the rest of the session for the same tool and command prefix or path pattern; or deny with an optional message returned to the model. Session approvals MUST NOT apply to destructive commands. Prompts for `write` and `edit` MUST show the diff.

#### Scenario: Approve for session
- **WHEN** the user approves `cargo test` for the session and the model later runs `cargo test --all`
```

with:

```markdown
- **THEN** the user is asked to approve before the file is read

### Requirement: Approval prompts offer once, session, or deny with feedback
When approval is required interactively, the system SHALL offer: approve once; approve for the rest of the session for the same tool and command prefix or path pattern; or deny with an optional message returned to the model. Session approvals MUST NOT apply to destructive commands, and a session approval that cannot apply MUST be reported as applying once. Prompts for `write` and `edit` MUST show the diff. A prompt to run a command outside the sandbox MUST NOT offer approval for the session.

#### Scenario: Approve for session
- **WHEN** the user approves `cargo test` for the session and the model later runs `cargo test --all`
```

Replace (2 of 4):

```markdown
#### Scenario: Session approval is scoped to the command prefix
- **WHEN** the user approves `git status` for the session and the model later runs `git push`
- **THEN** `git push` still requires approval

### Requirement: Non-interactive runs deny actions that need approval
When no user can answer an approval prompt, the system SHALL deny the action, tell the model it was denied for lack of approval, and record that an action was blocked.
```

with:

```markdown
#### Scenario: Session approval is scoped to the command prefix
- **WHEN** the user approves `git status` for the session and the model later runs `git push`
- **THEN** `git push` still requires approval

#### Scenario: A destructive command approved for the session
- **WHEN** the user approves `git reset --hard HEAD~1` for the session
- **THEN** it runs once, the user is told the approval applied once, and the next `git reset --hard` asks again

### Requirement: Non-interactive runs deny actions that need approval
When no user can answer an approval prompt, the system SHALL deny the action, tell the model it was denied for lack of approval, and record that an action was blocked.
```

Replace (3 of 4):

```markdown

On macOS, and on Linux when unprivileged user namespaces are available (the full tier), writes to these paths MUST fail. On Linux, names that do not exist yet MUST be caught by a guard that moves them to a quarantine directory, never deleting them, and reports it in the tool result. The guard's scan for new repositories MUST use the ignore rules as they were when the session started, so that an ignore rule written during the session cannot hide a new repository.

When user namespaces are unavailable (the basic tier), the system MUST warn at startup and point to `harness sandbox doctor`. The guard MUST restore changed protected files after each command, and, while a process started by an earlier sandboxed command is still running, also before each later command. When harness exits, it MUST end the processes sandboxed commands left running and check once more. With `sandbox.linux_git_protection = "required"`, the basic tier MUST require approval for every shell command in `ask` and `auto`.

#### Scenario: Planting a hook
- **WHEN** a sandboxed command runs `echo x > .git/hooks/pre-commit` in `auto` mode on macOS, or on Linux in the full tier
```

with:

```markdown

On macOS, and on Linux when unprivileged user namespaces are available (the full tier), writes to these paths MUST fail. On Linux, names that do not exist yet MUST be caught by a guard that moves them to a quarantine directory, never deleting them, and reports it in the tool result. The guard's scan for new repositories MUST use the ignore rules as they were when the session started, so that an ignore rule written during the session cannot hide a new repository.

When user namespaces are unavailable (the basic tier), the system MUST warn at startup and point to `harness sandbox doctor`. The guard MUST restore changed protected files after each command, and, while a process started by an earlier sandboxed command is still running, also before each later command. When harness exits, it MUST end the processes sandboxed commands left running and check once more. With `sandbox.linux_git_protection = "required"`, the basic tier MUST require approval for every shell command in `ask` and `auto`, including in a session that drops to the basic tier after it started: such a command then runs outside the sandbox only once approved, and a headless run MUST count it as blocked.

#### Scenario: Planting a hook
- **WHEN** a sandboxed command runs `echo x > .git/hooks/pre-commit` in `auto` mode on macOS, or on Linux in the full tier
```

Replace (4 of 4):

```markdown
- **WHEN** user namespaces are blocked, `sandbox.linux_git_protection = "required"`, and the model runs `ls` in `auto` mode
- **THEN** the user is asked to approve it

### Requirement: Commands run in bash without startup files
The system SHALL run shell commands with `bash --noprofile --norc -c` with `BASH_ENV` and `ENV` removed from the environment. Bash MUST be looked for only at `/bin/bash`, `/usr/bin/bash`, and `/run/current-system/sw/bin/bash`, never on `PATH`, and `/bin/sh -c` MUST be used only when none of them exists.

```

with:

```markdown
- **WHEN** user namespaces are blocked, `sandbox.linux_git_protection = "required"`, and the model runs `ls` in `auto` mode
- **THEN** the user is asked to approve it

#### Scenario: Strict git protection after a drop to the basic tier
- **WHEN** a session in the full tier with `sandbox.linux_git_protection = "required"` drops to the basic tier and the model then runs `ls` in `auto` mode
- **THEN** the user is asked whether to run it outside the sandbox, and a headless run exits with code 3 without running it

### Requirement: Commands run in bash without startup files
The system SHALL run shell commands with `bash --noprofile --norc -c` with `BASH_ENV` and `ENV` removed from the environment. Bash MUST be looked for only at `/bin/bash`, `/usr/bin/bash`, and `/run/current-system/sw/bin/bash`, never on `PATH`, and `/bin/sh -c` MUST be used only when none of them exists.

```

In `openspec/changes/add-core-agent/specs/configuration/spec.md`:

Replace:

```markdown
- **THEN** harness reports the unknown key with its file and line

### Requirement: Widening project settings require workspace trust
The system SHALL apply project-level settings that widen what the agent may do (a `mode` that grants more than the effective global mode; a `max_steps` above the effective global limit; `model`; `allow` rules; `read_dirs`; provider definitions or `base_url` overrides; `[sandbox]` settings other than `allow_localhost = false` and `linux_git_protection = "required"`) only when the user has trusted the workspace with the current set of those settings. The effective global mode is the global config's `mode`, or else the default mode for the workspace, with modes ranked `plan` = `read-only` < `ask` < `auto` < `full-access`; the effective global limit is the global `max_steps`, or else the built-in default. Trust MUST be recorded in the data directory as a fingerprint of the widening settings; when they change, the workspace MUST be treated as untrusted until trusted again. On first interactive use of a workspace with such settings, the system MUST display them and ask whether to trust the workspace. Headless runs MUST ignore untrusted widening settings with a warning. Narrowing settings (`deny`, `confirm`, a `mode` or `max_steps` no wider than the effective global one, `allow_localhost = false`, and `linux_git_protection = "required"`) MUST always apply.

#### Scenario: Cloned repository redirects a provider
- **WHEN** a cloned repository's `.harness/config.toml` sets `providers.openai.base_url` to an unknown host and the workspace is not trusted
- **THEN** harness shows the setting and asks for trust before using it
- **AND** until trusted, requests to `openai` go to the default endpoint

#### Scenario: Deny rules apply without trust
- **WHEN** an untrusted project config contains `deny = ["bash:git push*"]`
```

with:

```markdown
- **THEN** harness reports the unknown key with its file and line

### Requirement: Widening project settings require workspace trust
The system SHALL apply project-level settings that widen what the agent may do (a `mode` that grants more than the effective global mode; a `max_steps` above the effective global limit; `model`; `allow` rules; `read_dirs`; provider definitions or `base_url` overrides; `[sandbox]` settings other than `allow_localhost = false` and `linux_git_protection = "required"`) only when the user has trusted the workspace with the current set of those settings. The effective global mode is the global config's `mode`, or else the default mode for the workspace, with modes ranked `plan` = `read-only` < `ask` < `auto` < `full-access`; the effective global limit is the global `max_steps`, or else the built-in default. Trust MUST be recorded in the data directory as a fingerprint of the widening settings; when they change, the workspace MUST be treated as untrusted until trusted again. On first interactive use of a workspace with such settings, the system MUST display them and ask whether to trust the workspace; trusting MUST record trust as `harness trust` does, so the settings apply to that session, and declining MUST leave them unapplied and ask again at the next interactive start. Headless runs MUST ignore untrusted widening settings with a warning. Narrowing settings (`deny`, `confirm`, a `mode` or `max_steps` no wider than the effective global one, `allow_localhost = false`, and `linux_git_protection = "required"`) MUST always apply.

#### Scenario: Cloned repository redirects a provider
- **WHEN** a cloned repository's `.harness/config.toml` sets `providers.openai.base_url` to an unknown host and the workspace is not trusted
- **THEN** harness shows the setting and asks for trust before using it
- **AND** until trusted, requests to `openai` go to the default endpoint

#### Scenario: Trusting on first interactive use
- **WHEN** the user starts `harness --mode ask` in a cloned repository whose project config has `allow = ["bash:make *"]`, and answers yes
- **THEN** `make test` runs without an approval prompt in that session, and later sessions do not ask about trust again

#### Scenario: Deny rules apply without trust
- **WHEN** an untrusted project config contains `deny = ["bash:git push*"]`
```

- [ ] **Step 3: Name this plan in tasks.md**

In `openspec/changes/add-core-agent/tasks.md`:

Replace:

```markdown
- [ ] 4.6 Text tool-call recovery and truncation handling; verify the matching model-providers scenarios as tests
- [ ] 4.7 Secret-redaction audit across logs, sessions, tool output, and NDJSON; verify with a canary-key test

## 5. P5 Terminal UI (plan written after P4)

- [ ] 5.1 Inline renderer with native scrollback, Markdown and diff rendering, `NO_COLOR`; verify with ratatui `TestBackend` snapshots
- [ ] 5.2 Input editor: history, multi-line, collapsed pastes, `/` and `@` completion; verify with snapshot and unit tests
```

with:

```markdown
- [ ] 4.6 Text tool-call recovery and truncation handling; verify the matching model-providers scenarios as tests
- [ ] 4.7 Secret-redaction audit across logs, sessions, tool output, and NDJSON; verify with a canary-key test

## 5. P5 Terminal UI (first half, P5a: `docs/superpowers/plans/2026-09-28-m1-p5a-terminal-ui.md`, written before P4 by the human's choice: 5.1 to 5.6 and 5.8, with the first-use trust prompt and the 2.13 final review's M1 and M4; the rest, 5.7 and what needs P4, is planned after P4)

- [ ] 5.1 Inline renderer with native scrollback, Markdown and diff rendering, `NO_COLOR`; verify with ratatui `TestBackend` snapshots
- [ ] 5.2 Input editor: history, multi-line, collapsed pastes, `/` and `@` completion; verify with snapshot and unit tests
```

- [ ] **Step 4: Validate**

Run: `openspec validate add-core-agent --strict`
Expected: `Change 'add-core-agent' is valid`.

- [ ] **Step 5: Commit**

```bash
git add openspec
git commit -F - <<'EOF'
docs(spec): write the P5a refinements into the spec

Design D1 gains the harness-tui crate and the Steered event; D5 says a
session that drops to the Linux basic tier with "required" asks too;
D7 lets the interactive session continue a session; D8 settles the
terminal UI; D9 describes the first-use trust prompt; D12 how plan mode
ends and where the approved plan is kept. Five specs follow, and
tasks.md names this plan.

<trailer lines from the controller>
EOF
```

---

### Task 2: `harness-tui`: Markdown, code and diffs

**Files:**
- Modify: `Cargo.toml`, `deny.toml`
- Create: `crates/harness-tui/Cargo.toml`, `crates/harness-tui/src/{lib,style,text,highlight,diff,markdown}.rs`
- Test: `crates/harness-tui/tests/render.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `style::Theme { color: bool, truecolor: bool }` with `from_env()`, `from_vars(no_color: Option<&str>, colorterm: Option<&str>)`, `colored()`, `monochrome()`, styles (`plain`, `bold`, `dim`, `italic`, `user`, `accent`, `heading`, `code`, `link`, `quote`, `added`, `removed`, `hunk`, `error`, `warning`, `selected`) and `rgb(r, g, b) -> Option<Color>`;
  - `text::sanitize(&str) -> String` (control and bidirectional characters as escapes, tabs as four spaces, `\n` kept), `text::width(&str) -> usize`, `text::lines(&str, Style) -> Vec<Line<'static>>`, `text::wrap(&Line, width, first: &[Span], rest: &[Span]) -> Vec<Line<'static>>`, `text::plain(&Line) -> String`;
  - `markdown::render(markdown: &str, width: usize, theme: &Theme) -> Vec<Line<'static>>`;
  - `highlight::highlight(code: &str, language: &str, theme: &Theme) -> Option<Vec<Line<'static>>>`;
  - `diff::unified(old: &str, new: &str, context: usize, theme: &Theme) -> Vec<Line<'static>>` and `diff::counts(old, new) -> (added, removed)`.

- [ ] **Step 1: Write the failing tests**

Add the crates to the workspace. In `Cargo.toml`:

Replace (1 of 3):

```toml
harness-sandbox = { path = "crates/harness-sandbox" }
harness-shell = { path = "crates/harness-shell" }
harness-tools = { path = "crates/harness-tools" }
async-stream = "0.3"
async-trait = "0.1.92"
clap = { version = "4.6.7", features = ["derive"] }
```

with:

```toml
harness-sandbox = { path = "crates/harness-sandbox" }
harness-shell = { path = "crates/harness-shell" }
harness-tools = { path = "crates/harness-tools" }
harness-tui = { path = "crates/harness-tui" }
async-stream = "0.3"
async-trait = "0.1.92"
clap = { version = "4.6.7", features = ["derive"] }
```

Replace (2 of 3):

```toml
libc = "0.2"
jsonschema = "0.57.0"
nix = { version = "0.31.3", features = ["signal", "process"] }
regex = "1"
reqwest = { version = "0.13.5", default-features = false, features = ["json", "stream", "rustls"] }
seccompiler = "=0.5.0"
```

with:

```toml
libc = "0.2"
jsonschema = "0.57.0"
nix = { version = "0.31.3", features = ["signal", "process"] }
pulldown-cmark = { version = "0.13.4", default-features = false }
ratatui = { version = "0.30.2", default-features = false, features = ["crossterm"] }
regex = "1"
reqwest = { version = "0.13.5", default-features = false, features = ["json", "stream", "rustls"] }
seccompiler = "=0.5.0"
```

Replace (3 of 3):

```toml
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
```

with:

```toml
serde_json = "1.0.151"
sha2 = "0.11.0"
similar = "3.2.0"
syntect = { version = "5.3.0", default-features = false, features = ["parsing", "default-syntaxes", "default-themes", "regex-fancy"] }
thiserror = "2.0.21"
tokio = { version = "1.53.1", features = ["full"] }
tokio-util = "0.7.19"
toml = "1.1.6"
unicode-width = "0.2.2"
assert_cmd = "2.2.2"
predicates = "3.1.4"
tempfile = "3.27.0"
```

Create `crates/harness-tui/Cargo.toml`:

```toml
[package]
name = "harness-tui"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
publish.workspace = true

[dependencies]
pulldown-cmark.workspace = true
ratatui.workspace = true
similar.workspace = true
syntect.workspace = true
unicode-width.workspace = true
```

Create `crates/harness-tui/src/lib.rs`, for now with its doc comment only:

```rust
//! harness's inline terminal UI: the conversation goes into the terminal's own scrollback, and
//! only a small live region at the bottom (the input, what is streaming, prompts) is redrawn.
//! This crate draws with `ratatui`; `harness-cli` sets up the session and starts it.
```

Create `crates/harness-tui/tests/render.rs`:

````rust
//! Markdown, code and diffs as they appear on screen, drawn into ratatui's `TestBackend`.

use harness_tui::{diff, markdown, style::Theme, text};
use ratatui::{
    Terminal,
    backend::TestBackend,
    buffer::Buffer,
    style::Color,
    text::{Line, Span},
    widgets::Paragraph,
};

/// `lines` drawn on a screen `width` columns wide and as tall as they are.
fn draw(lines: Vec<Line<'static>>, width: u16) -> Buffer {
    let height = (lines.len() as u16).max(1);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| frame.render_widget(Paragraph::new(lines), frame.area()))
        .unwrap();
    terminal.backend().buffer().clone()
}

/// Each row of `buffer` as text, without trailing spaces.
fn rows(buffer: &Buffer) -> Vec<String> {
    let width = buffer.area.width as usize;
    buffer
        .content
        .chunks(width)
        .map(|row| {
            let text: String = row.iter().map(|cell| cell.symbol()).collect();
            text.trim_end().to_string()
        })
        .collect()
}

fn colours(buffer: &Buffer) -> Vec<Color> {
    buffer
        .content
        .iter()
        .flat_map(|cell| [cell.fg, cell.bg])
        .filter(|c| *c != Color::Reset)
        .collect()
}

const SAMPLE: &str = "\
# Plan

Add a **rate limiter** to the login handler, so that repeated failures slow down.

- read `src/login.rs`
- change it:
  1. count failures
  2. sleep after three
> Keep the old behaviour behind a flag.

```rust
fn main() {}
```

| name | lines |
|------|-------|
| a.rs | 12 |

See [the docs](https://example.com/docs).
";

#[test]
fn markdown_renders_headings_lists_code_quotes_and_tables_at_a_width() {
    let buffer = draw(markdown::render(SAMPLE, 40, &Theme::colored()), 40);
    assert_eq!(
        rows(&buffer),
        [
            "Plan",
            "",
            "Add a rate limiter to the login handler,",
            "so that repeated failures slow down.",
            "",
            "- read src/login.rs",
            "- change it:",
            "  1. count failures",
            "  2. sleep after three",
            "",
            "│ Keep the old behaviour behind a flag.",
            "",
            "  fn main() {}",
            "",
            "name │ lines",
            "─────┼──────",
            "a.rs │ 12",
            "",
            "See the docs (https://example.com/docs).",
        ]
    );
}

#[test]
fn long_words_and_list_items_wrap_under_their_text() {
    let lines = markdown::render(
        "- one two three four five six seven\n- abcdefghijklmnopqrstuvwxyz",
        16,
        &Theme::colored(),
    );
    assert_eq!(
        rows(&draw(lines, 16)),
        [
            "- one two three",
            "  four five six",
            "  seven",
            "- abcdefghijklmn",
            "  opqrstuvwxyz",
        ]
    );
}

#[test]
fn code_blocks_are_highlighted_only_with_colour_and_a_known_language() {
    let code = "```rust\nlet x = \"hi\";\n```\n";
    let coloured = draw(markdown::render(code, 30, &Theme::colored()), 30);
    let distinct: std::collections::HashSet<_> = colours(&coloured).into_iter().collect();
    assert!(distinct.len() >= 2, "{distinct:?}");
    assert!(distinct.iter().all(|c| matches!(c, Color::Rgb(..))));
    // Without 24-bit colour, the nearest of the 256 standard colours.
    let theme = Theme::from_vars(None, None);
    let indexed = draw(markdown::render(code, 30, &theme), 30);
    assert!(
        colours(&indexed)
            .iter()
            .all(|c| matches!(c, Color::Indexed(_)))
    );
    // An unknown language is shown as it is.
    let unknown = draw(
        markdown::render("```nosuchlang\nlet x = 1;\n```\n", 30, &Theme::colored()),
        30,
    );
    assert!(colours(&unknown).is_empty());
    assert_eq!(rows(&unknown), ["  let x = 1;"]);
}

#[test]
fn no_color_draws_no_colour_and_keeps_code_and_diff_markers() {
    let theme = Theme::from_vars(Some("1"), Some("truecolor"));
    assert!(!theme.color);
    let buffer = draw(markdown::render(SAMPLE, 40, &theme), 40);
    assert!(colours(&buffer).is_empty(), "{:?}", colours(&buffer));
    assert!(rows(&buffer).contains(&"- read `src/login.rs`".to_string()));
    let diff = draw(diff::unified("a\nb\n", "a\nc\n", 1, &theme), 20);
    assert!(colours(&diff).is_empty());
    assert_eq!(rows(&diff), ["@@ -1,2 +1,2 @@", " a", "-b", "+c"]);
    // An empty NO_COLOR does not count.
    assert!(Theme::from_vars(Some(""), None).color);
}

#[test]
fn a_diff_shows_hunks_with_coloured_marked_lines() {
    let theme = Theme::colored();
    let old = "one\ntwo\nthree\nfour\nfive\nsix\nseven\n";
    let new = "one\n2\nthree\nfour\nfive\nsix\nseven\neight\n";
    let buffer = draw(diff::unified(old, new, 1, &theme), 20);
    assert_eq!(
        rows(&buffer),
        [
            "@@ -1,3 +1,3 @@",
            " one",
            "-two",
            "+2",
            " three",
            "@@ -7 +7,2 @@",
            " seven",
            "+eight",
        ]
    );
    let row = |y: u16| &buffer[(0, y)];
    assert_eq!(row(2).fg, Color::Red);
    assert_eq!(row(3).fg, Color::Green);
    assert_eq!(diff::counts(old, new), (2, 1));
    assert!(diff::unified("same\n", "same\n", 3, &theme).is_empty());
}

#[test]
fn control_characters_from_the_model_are_shown_escaped() {
    let buffer = draw(
        markdown::render(
            "evil \u{1b}[2J text \u{202e}reversed and a \u{7} bell\n\n```\n\u{1b}]0;title\u{7}\n```",
            60,
            &Theme::colored(),
        ),
        60,
    );
    let screen = rows(&buffer).join("\n");
    assert!(
        !screen.chars().any(|c| c.is_control() && c != '\n'),
        "{screen:?}"
    );
    assert!(
        screen.contains("evil \\u{1b}[2J text \\u{202e}reversed"),
        "{screen}"
    );
    assert!(screen.contains("\\u{1b}]0;title\\u{7}"), "{screen}");
    assert_eq!(text::sanitize("a\tb\r\nc"), "a    b\nc");
}

#[test]
fn wrapping_keeps_styles_and_prefixes() {
    let line = Line::from(vec![
        Span::styled(
            "red words ",
            ratatui::style::Style::default().fg(Color::Red),
        ),
        Span::raw("plain words here"),
    ]);
    let wrapped = text::wrap(&line, 12, &[Span::raw("> ")], &[Span::raw("  ")]);
    let texts: Vec<String> = wrapped.iter().map(text::plain).collect();
    assert_eq!(texts, ["> red words", "  plain", "  words here"]);
    assert_eq!(wrapped[0].spans[1].style.fg, Some(Color::Red));
    assert_eq!(text::width("日本"), 4);
}
````

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p harness-tui --test render`
Expected: FAIL to compile: ``unresolved imports `harness_tui::diff`, `harness_tui::markdown`, `harness_tui::style`, `harness_tui::text` ``.

- [ ] **Step 3: Styles and NO_COLOR**

Create `crates/harness-tui/src/style.rs`:

```rust
//! The styles the terminal UI draws with. With `NO_COLOR` set to anything but an empty string
//! (<https://no-color.org>), no colour is used at all: text keeps only bold, dim, italic,
//! underline and reverse, and diffs keep their `+` and `-` markers.

use ratatui::style::{Color, Modifier, Style};

/// Whether and how the UI uses colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// Colours are used at all.
    pub color: bool,
    /// The terminal shows 24-bit colours (`COLORTERM=truecolor` or `24bit`); otherwise syntax
    /// highlighting uses the nearest of the 256 standard colours.
    pub truecolor: bool,
}

impl Theme {
    /// The theme for this process's environment: `NO_COLOR` and `COLORTERM`.
    pub fn from_env() -> Theme {
        Theme::from_vars(
            std::env::var("NO_COLOR").ok().as_deref(),
            std::env::var("COLORTERM").ok().as_deref(),
        )
    }

    /// The theme for these values of `NO_COLOR` and `COLORTERM`.
    pub fn from_vars(no_color: Option<&str>, colorterm: Option<&str>) -> Theme {
        Theme {
            color: no_color.is_none_or(str::is_empty),
            truecolor: matches!(colorterm, Some("truecolor" | "24bit")),
        }
    }

    /// Colour and 24-bit colour.
    pub fn colored() -> Theme {
        Theme {
            color: true,
            truecolor: true,
        }
    }

    /// No colour at all, as with `NO_COLOR`.
    pub fn monochrome() -> Theme {
        Theme {
            color: false,
            truecolor: false,
        }
    }

    fn fg(&self, style: Style, color: Color) -> Style {
        if self.color { style.fg(color) } else { style }
    }

    pub fn plain(&self) -> Style {
        Style::default()
    }

    pub fn bold(&self) -> Style {
        Style::default().add_modifier(Modifier::BOLD)
    }

    pub fn dim(&self) -> Style {
        Style::default().add_modifier(Modifier::DIM)
    }

    pub fn italic(&self) -> Style {
        Style::default().add_modifier(Modifier::ITALIC)
    }

    /// What the user typed.
    pub fn user(&self) -> Style {
        self.fg(self.bold(), Color::Cyan)
    }

    /// The prompt marker and other accents.
    pub fn accent(&self) -> Style {
        self.fg(self.bold(), Color::Magenta)
    }

    pub fn heading(&self) -> Style {
        self.fg(self.bold(), Color::Cyan)
    }

    /// Inline code.
    pub fn code(&self) -> Style {
        self.fg(Style::default(), Color::Yellow)
    }

    pub fn link(&self) -> Style {
        self.fg(
            Style::default().add_modifier(Modifier::UNDERLINED),
            Color::Blue,
        )
    }

    pub fn quote(&self) -> Style {
        self.fg(self.italic(), Color::Gray)
    }

    /// A diff's added lines.
    pub fn added(&self) -> Style {
        self.fg(Style::default(), Color::Green)
    }

    /// A diff's removed lines.
    pub fn removed(&self) -> Style {
        self.fg(Style::default(), Color::Red)
    }

    /// A diff's hunk headers.
    pub fn hunk(&self) -> Style {
        self.fg(self.dim(), Color::Cyan)
    }

    pub fn error(&self) -> Style {
        self.fg(self.bold(), Color::Red)
    }

    pub fn warning(&self) -> Style {
        self.fg(Style::default(), Color::Yellow)
    }

    /// Something selected in a list.
    pub fn selected(&self) -> Style {
        if self.color {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    /// A 24-bit colour from syntax highlighting, as this terminal can show it.
    pub fn rgb(&self, r: u8, g: u8, b: u8) -> Option<Color> {
        if !self.color {
            None
        } else if self.truecolor {
            Some(Color::Rgb(r, g, b))
        } else {
            Some(Color::Indexed(xterm256(r, g, b)))
        }
    }
}

/// The nearest of the 256 standard terminal colours: the 6×6×6 colour cube or the 24 greys.
fn xterm256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let nearest = |v: u8| {
        LEVELS
            .iter()
            .enumerate()
            .min_by_key(|(_, level)| (i32::from(**level) - i32::from(v)).abs())
            .map(|(i, _)| i as u8)
            .unwrap_or(0)
    };
    let (ri, gi, bi) = (nearest(r), nearest(g), nearest(b));
    let cube = 16 + 36 * ri + 6 * gi + bi;
    let distance = |x: [u8; 3]| -> i32 {
        [r, g, b]
            .iter()
            .zip(x)
            .map(|(a, b)| (i32::from(*a) - i32::from(b)).pow(2))
            .sum()
    };
    let cube_rgb = [
        LEVELS[ri as usize],
        LEVELS[gi as usize],
        LEVELS[bi as usize],
    ];
    let average = (u16::from(r) + u16::from(g) + u16::from(b)) / 3;
    let grey_index = (average.saturating_sub(3) / 10).min(23) as u8;
    let grey = 8 + 10 * grey_index;
    if distance([grey, grey, grey]) < distance(cube_rgb) {
        232 + grey_index
    } else {
        cube
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_colours_map_to_the_cube_and_greys() {
        assert_eq!(xterm256(0, 0, 0), 16);
        assert_eq!(xterm256(255, 255, 255), 231);
        assert_eq!(xterm256(255, 0, 0), 196);
        assert_eq!(xterm256(128, 128, 128), 244);
    }
}
```

- [ ] **Step 4: Sanitizing and wrapping**

Create `crates/harness-tui/src/text.rs`:

```rust
//! Text from the model, tools, files and the user, made safe to draw, and wrapped to a width.

use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthChar;

/// Columns a tab takes.
const TAB: &str = "    ";

/// `text` made safe to draw: every control character, and every character that reorders text on
/// screen (bidirectional marks, embeddings, overrides and isolates), is shown as an escape such as
/// `\u{1b}`, so it cannot move the cursor or disguise what is shown. A tab becomes four spaces;
/// `\n` is kept, and `\r\n` becomes `\n`.
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' => out.push('\n'),
            '\t' => out.push_str(TAB),
            '\r' if chars.peek() == Some(&'\n') => {}
            c if c.is_control() || is_bidi_control(c) => out.extend(c.escape_default()),
            c => out.push(c),
        }
    }
    out
}

fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

/// Columns `text` takes on screen.
pub fn width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// One line per line of `text` (sanitized), each in `style`.
pub fn lines(text: &str, style: Style) -> Vec<Line<'static>> {
    sanitize(text)
        .split('\n')
        .map(|l| Line::from(Span::styled(l.to_string(), style)))
        .collect()
}

/// `line` broken into lines at most `width` columns wide: between words where it can, inside a
/// word where it must. The first line starts with `first`, the others with `rest`, which count
/// towards the width. Spaces where a line breaks are dropped.
pub fn wrap(
    line: &Line<'_>,
    width: usize,
    first: &[Span<'static>],
    rest: &[Span<'static>],
) -> Vec<Line<'static>> {
    let prefix_width = |p: &[Span<'static>]| p.iter().map(|s| self::width(&s.content)).sum();
    let chars: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|span| {
            let style = line.style.patch(span.style);
            span.content.chars().map(move |c| (c, style))
        })
        .collect();
    let mut out = Vec::new();
    let mut current: Vec<(char, Style)> = Vec::new();
    let mut used = 0;
    let mut available = width.saturating_sub(prefix_width(first)).max(1);
    let finish = |current: &mut Vec<(char, Style)>, out: &mut Vec<Line<'static>>| {
        while current.last().is_some_and(|(c, _)| *c == ' ') {
            current.pop();
        }
        let prefix = if out.is_empty() { first } else { rest };
        let mut spans = prefix.to_vec();
        spans.extend(merge(current));
        out.push(Line::from(spans));
        current.clear();
    };
    let mut i = 0;
    while i < chars.len() {
        // The next word and the spaces after it.
        let start = i;
        while i < chars.len() && chars[i].0 != ' ' {
            i += 1;
        }
        let word_end = i;
        while i < chars.len() && chars[i].0 == ' ' {
            i += 1;
        }
        let word: usize = chars[start..word_end]
            .iter()
            .map(|(c, _)| c.width().unwrap_or(0))
            .sum();
        if used + word > available && used > 0 {
            finish(&mut current, &mut out);
            used = 0;
            available = width.saturating_sub(prefix_width(rest)).max(1);
        }
        for &(c, style) in &chars[start..i] {
            let w = c.width().unwrap_or(0);
            if c == ' ' && used == 0 && !current.is_empty() {
                continue;
            }
            if used + w > available {
                if c == ' ' {
                    continue;
                }
                finish(&mut current, &mut out);
                used = 0;
                available = width.saturating_sub(prefix_width(rest)).max(1);
            }
            current.push((c, style));
            used += w;
        }
    }
    finish(&mut current, &mut out);
    out
}

/// Consecutive characters of the same style as one span each.
fn merge(chars: &[(char, Style)]) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for &(c, style) in chars {
        match spans.last_mut() {
            Some(last) if last.style == style => last.content.to_mut().push(c),
            _ => spans.push(Span::styled(c.to_string(), style)),
        }
    }
    spans
}

/// The text of `line`, without styles.
pub fn plain(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}
```

- [ ] **Step 5: Code highlighting and diffs**

Create `crates/harness-tui/src/highlight.rs`:

```rust
//! Syntax highlighting for code blocks, with syntect's bundled syntaxes and theme. They are
//! loaded the first time a code block needs them.

use std::sync::OnceLock;

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Theme as SyntaxTheme, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};

use crate::{style::Theme, text::sanitize};

/// Lines longer than this are not highlighted: syntect's regular expressions can take a long
/// time on them.
const MAX_LINE: usize = 2_000;

struct Assets {
    syntaxes: SyntaxSet,
    theme: SyntaxTheme,
}

fn assets() -> &'static Assets {
    static ASSETS: OnceLock<Assets> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let mut themes = ThemeSet::load_defaults();
        Assets {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            theme: themes
                .themes
                .remove("base16-ocean.dark")
                .unwrap_or_default(),
        }
    })
}

/// `code` highlighted as `language`, the word after a code fence's backticks (`rust`, `py`,
/// `sh`, ...), one line per line of code. `None` when the theme has no colour, the language is
/// unknown, or a line is too long to highlight.
pub fn highlight(code: &str, language: &str, theme: &Theme) -> Option<Vec<Line<'static>>> {
    if !theme.color || language.is_empty() || code.lines().any(|l| l.len() > MAX_LINE) {
        return None;
    }
    let assets = assets();
    let syntax = assets.syntaxes.find_syntax_by_token(language)?;
    let mut highlighter = HighlightLines::new(syntax, &assets.theme);
    let mut out = Vec::new();
    for line in LinesWithEndings::from(code) {
        let ranges = highlighter.highlight_line(line, &assets.syntaxes).ok()?;
        let spans = ranges
            .into_iter()
            .map(|(style, text)| {
                let text = sanitize(text.trim_end_matches(['\n', '\r']));
                let mut out = Style::default();
                let fg = style.foreground;
                if let Some(color) = theme.rgb(fg.r, fg.g, fg.b) {
                    out = out.fg(color);
                }
                if style.font_style.contains(FontStyle::BOLD) {
                    out = out.add_modifier(Modifier::BOLD);
                }
                if style.font_style.contains(FontStyle::ITALIC) {
                    out = out.add_modifier(Modifier::ITALIC);
                }
                Span::styled(text, out)
            })
            .filter(|span| !span.content.is_empty())
            .collect::<Vec<_>>();
        out.push(Line::from(spans));
    }
    Some(out)
}
```

Create `crates/harness-tui/src/diff.rs`:

```rust
//! Diffs of a file's old and new text, as a unified diff with coloured `-` and `+` lines.

use std::time::Duration;

use ratatui::text::{Line, Span};
use similar::{ChangeTag, TextDiff};

use crate::{style::Theme, text::sanitize};

/// How long the diff may take before it settles for a coarser answer.
const TIMEOUT: Duration = Duration::from_millis(500);

/// A unified diff of `old` and `new`, with `context` unchanged lines around each change:
/// `@@ -a,b +c,d @@` headers, then lines marked ` `, `-` and `+`. Empty when they are equal.
pub fn unified(old: &str, new: &str, context: usize, theme: &Theme) -> Vec<Line<'static>> {
    let diff = TextDiff::configure().timeout(TIMEOUT).diff_lines(old, new);
    let mut out = Vec::new();
    for hunk in diff.unified_diff().context_radius(context).iter_hunks() {
        out.push(Line::from(Span::styled(
            hunk.header().to_string(),
            theme.hunk(),
        )));
        for change in hunk.iter_changes() {
            let (marker, style) = match change.tag() {
                ChangeTag::Delete => ("-", theme.removed()),
                ChangeTag::Insert => ("+", theme.added()),
                ChangeTag::Equal => (" ", theme.dim()),
            };
            let text = sanitize(change.value().trim_end_matches(['\n', '\r']));
            out.push(Line::from(Span::styled(format!("{marker}{text}"), style)));
        }
    }
    out
}

/// How many lines `new` adds to `old`, and how many it removes.
pub fn counts(old: &str, new: &str) -> (usize, usize) {
    let diff = TextDiff::configure().timeout(TIMEOUT).diff_lines(old, new);
    let mut added = 0;
    let mut removed = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    (added, removed)
}
```

- [ ] **Step 6: Markdown**

Create `crates/harness-tui/src/markdown.rs`:

```rust
//! Markdown, as the model writes it, rendered as styled lines of a given width: paragraphs,
//! headings, emphasis, inline code, fenced code blocks with syntax highlighting, lists, block
//! quotes, links, tables and rules. Everything drawn is sanitized first.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use crate::{
    highlight::highlight,
    style::Theme,
    text::{sanitize, width as text_width, wrap},
};

/// `markdown` as lines at most `width` columns wide.
pub fn render(markdown: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    let mut renderer = Renderer {
        theme,
        width: width.max(8),
        out: Vec::new(),
        line: Vec::new(),
        styles: Vec::new(),
        containers: Vec::new(),
        code: None,
        table: None,
        links: Vec::new(),
        needs_blank: false,
    };
    for event in Parser::new_ext(markdown, options) {
        renderer.event(event);
    }
    renderer.flush();
    renderer.out
}

enum Container {
    Quote,
    List { next: Option<u64> },
    Item { marker: String, first: bool },
}

struct Code {
    language: String,
    text: String,
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: String,
    head_rows: usize,
}

struct Renderer<'t> {
    theme: &'t Theme,
    width: usize,
    out: Vec<Line<'static>>,
    /// The line being built.
    line: Vec<Span<'static>>,
    /// Inline styles that apply now, innermost last.
    styles: Vec<Style>,
    containers: Vec<Container>,
    code: Option<Code>,
    table: Option<Table>,
    /// For each open link: its address, and where its text starts in `line`.
    links: Vec<(String, usize)>,
    /// A blank line goes before the next block.
    needs_blank: bool,
}

impl Renderer<'_> {
    fn style(&self) -> Style {
        self.styles
            .iter()
            .fold(Style::default(), |acc, s| acc.patch(*s))
    }

    fn push(&mut self, text: &str, style: Style) {
        let text = sanitize(text).replace('\n', " ");
        if text.is_empty() {
            return;
        }
        if let Some(table) = &mut self.table {
            table.cell.push_str(&text);
            return;
        }
        self.line.push(Span::styled(text, style));
    }

    /// The prefixes of the first line and of later lines, from the enclosing quotes and lists.
    fn prefixes(&self) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
        let mut first = Vec::new();
        let mut rest = Vec::new();
        for container in &self.containers {
            match container {
                Container::Quote => {
                    first.push(Span::styled("│ ", self.theme.quote()));
                    rest.push(Span::styled("│ ", self.theme.quote()));
                }
                Container::List { .. } => {}
                Container::Item {
                    marker,
                    first: at_start,
                } => {
                    let blank = " ".repeat(text_width(marker));
                    if *at_start {
                        first.push(Span::styled(marker.clone(), self.theme.accent()));
                    } else {
                        first.push(Span::raw(blank.clone()));
                    }
                    rest.push(Span::raw(blank));
                }
            }
        }
        (first, rest)
    }

    fn items_started(&mut self) {
        for container in &mut self.containers {
            if let Container::Item { first, .. } = container {
                *first = false;
            }
        }
    }

    /// Wraps and emits the line being built, if any.
    fn flush(&mut self) {
        if self.line.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.line);
        let (first, rest) = self.prefixes();
        self.out
            .extend(wrap(&Line::from(spans), self.width, &first, &rest));
        self.items_started();
    }

    /// Starts a block: a blank line after the previous one.
    fn block(&mut self) {
        self.flush();
        if self.needs_blank && !self.out.is_empty() {
            let (_, rest) = self.prefixes();
            let quotes: Vec<Span<'static>> = rest
                .into_iter()
                .filter(|s| s.content.trim() == "│")
                .collect();
            self.out.push(Line::from(quotes));
        }
        self.needs_blank = false;
    }

    fn in_item(&self) -> bool {
        self.containers
            .iter()
            .any(|c| matches!(c, Container::Item { .. }))
    }

    fn event(&mut self, event: Event<'_>) {
        if let Some(code) = &mut self.code {
            match event {
                Event::Text(text) => code.text.push_str(&text),
                Event::End(TagEnd::CodeBlock) => self.end_code(),
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.push(&text, self.style()),
            Event::Code(code) | Event::InlineMath(code) | Event::DisplayMath(code) => {
                let style = self.style().patch(self.theme.code());
                if self.theme.color {
                    self.push(&code, style);
                } else {
                    self.push(&format!("`{code}`"), style);
                }
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                self.push(html.trim_end_matches('\n'), self.theme.dim())
            }
            Event::FootnoteReference(label) => self.push(&format!("[^{label}]"), self.style()),
            Event::SoftBreak => self.push(" ", self.style()),
            Event::HardBreak => self.flush(),
            Event::Rule => {
                self.block();
                let (first, _) = self.prefixes();
                let used: usize = first.iter().map(|s| text_width(&s.content)).sum();
                let mut spans = first;
                spans.push(Span::styled(
                    "─".repeat(self.width.saturating_sub(used).min(40)),
                    self.theme.dim(),
                ));
                self.out.push(Line::from(spans));
                self.needs_blank = true;
            }
            Event::TaskListMarker(done) => {
                self.push(if done { "[x] " } else { "[ ] " }, self.theme.dim())
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.block(),
            Tag::Heading { level, .. } => {
                self.block();
                let style = if matches!(level, HeadingLevel::H1 | HeadingLevel::H2) {
                    self.theme.heading()
                } else {
                    self.theme.bold()
                };
                self.styles.push(style);
            }
            Tag::BlockQuote(_) => {
                self.block();
                self.containers.push(Container::Quote);
            }
            Tag::CodeBlock(kind) => {
                self.block();
                let language = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split([' ', ',', '{']).next().unwrap_or("").to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some(Code {
                    language,
                    text: String::new(),
                });
            }
            Tag::List(start) => {
                if self.in_item() {
                    self.flush();
                } else {
                    self.block();
                }
                self.containers.push(Container::List { next: start });
            }
            Tag::Item => {
                self.flush();
                let marker = match self.containers.last_mut() {
                    Some(Container::List { next: Some(n) }) => {
                        let marker = format!("{n}. ");
                        *n += 1;
                        marker
                    }
                    _ => "- ".to_string(),
                };
                self.containers.push(Container::Item {
                    marker,
                    first: true,
                });
            }
            Tag::Table(_) => {
                self.block();
                self.table = Some(Table::default());
            }
            Tag::TableHead | Tag::TableRow | Tag::TableCell => {}
            Tag::Emphasis => self.styles.push(self.theme.italic()),
            Tag::Strong => self.styles.push(self.theme.bold()),
            Tag::Strikethrough => self
                .styles
                .push(Style::default().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.links.push((dest_url.to_string(), self.line.len()));
                self.styles.push(self.theme.link());
            }
            Tag::Image { dest_url, .. } => {
                self.push("[image: ", self.theme.dim());
                self.links.push((dest_url.to_string(), self.line.len()));
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush();
                self.needs_blank = true;
            }
            TagEnd::Heading(_) => {
                self.styles.pop();
                self.flush();
                self.needs_blank = true;
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.containers.pop();
                self.needs_blank = true;
            }
            TagEnd::List(_) => {
                self.flush();
                self.containers.pop();
                if !self.in_item() {
                    self.needs_blank = true;
                }
            }
            TagEnd::Item => {
                self.flush();
                self.containers.pop();
            }
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table {
                    let cell = std::mem::take(&mut table.cell);
                    table.row.push(cell.trim().to_string());
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                    table.head_rows = table.rows.len();
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.end_table(table);
                }
                self.needs_blank = true;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                if let Some((url, start)) = self.links.pop() {
                    let text: String = self.line[start.min(self.line.len())..]
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect();
                    if !url.is_empty() && text != url && format!("mailto:{text}") != url {
                        self.push(&format!(" ({url})"), self.theme.dim());
                    }
                }
            }
            TagEnd::Image => {
                if let Some((url, _)) = self.links.pop() {
                    self.push(&format!("] ({url})"), self.theme.dim());
                }
            }
            _ => {}
        }
    }

    fn end_code(&mut self) {
        let Some(code) = self.code.take() else {
            return;
        };
        let text = code.text.strip_suffix('\n').unwrap_or(&code.text);
        let lines = highlight(text, &code.language, self.theme).unwrap_or_else(|| {
            text.split('\n')
                .map(|l| Line::from(Span::styled(sanitize(l), self.theme.plain())))
                .collect()
        });
        let (first, rest) = self.prefixes();
        let indent = Span::raw("  ");
        for (i, line) in lines.iter().enumerate() {
            let mut first_prefix = if i == 0 { first.clone() } else { rest.clone() };
            first_prefix.push(indent.clone());
            let mut rest_prefix = rest.clone();
            rest_prefix.push(indent.clone());
            self.out
                .extend(wrap(line, self.width, &first_prefix, &rest_prefix));
        }
        self.items_started();
        self.needs_blank = true;
    }

    fn end_table(&mut self, table: Table) {
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        let mut widths = vec![0; columns];
        for row in &table.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(text_width(cell));
            }
        }
        let (first, rest) = self.prefixes();
        let prefix_width: usize = rest.iter().map(|s| text_width(&s.content)).sum();
        let total: usize = widths.iter().sum::<usize>() + 3 * (columns - 1);
        let fits = prefix_width + total <= self.width;
        for (r, row) in table.rows.iter().enumerate() {
            let style = if r < table.head_rows {
                self.theme.bold()
            } else {
                self.theme.plain()
            };
            let mut spans = Vec::new();
            for (i, cell) in row.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(" │ ", self.theme.dim()));
                }
                let pad = if fits && i + 1 < row.len() {
                    widths[i] - text_width(cell)
                } else {
                    0
                };
                spans.push(Span::styled(format!("{cell}{}", " ".repeat(pad)), style));
            }
            let lead = if r == 0 { &first } else { &rest };
            self.out
                .extend(wrap(&Line::from(spans), self.width, lead, &rest));
            if fits && r + 1 == table.head_rows {
                let rule: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
                let mut spans = rest.clone();
                spans.push(Span::styled(rule.join("─┼─"), self.theme.dim()));
                self.out.push(Line::from(spans));
            }
        }
        self.items_started();
    }
}
```

Create `crates/harness-tui/src/lib.rs`, now with the modules:

```rust
//! harness's inline terminal UI: the conversation goes into the terminal's own scrollback, and
//! only a small live region at the bottom (the input, what is streaming, prompts) is redrawn.
//! This crate draws with `ratatui`; `harness-cli` sets up the session and starts it.

pub mod diff;
pub mod highlight;
pub mod markdown;
pub mod style;
pub mod text;
```

- [ ] **Step 7: Ignore bincode's advisory**

In `deny.toml`:

Replace:

```toml
[advisories]
version = 2
yanked = "deny"

[licenses]
version = 2
```

with:

```toml
[advisories]
version = 2
yanked = "deny"
# bincode 1.3.3 is unmaintained (its team calls it complete). syntect decodes with it only the
# syntax and theme dumps built into harness, never input from outside.
ignore = [
  { id = "RUSTSEC-2025-0141", reason = "syntect decodes only the syntax and theme dumps built into harness with bincode" },
]

[licenses]
version = 2
```

- [ ] **Step 8: Run the tests, lint, and check the dependencies**

Run: `cargo test -p harness-tui`
Expected: PASS (8 tests).

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo deny check`
Expected: clean; `cargo deny` ends with `advisories ok, bans ok, licenses ok, sources ok`.

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml Cargo.lock deny.toml crates/harness-tui
git commit -F - <<'EOF'
feat(tui): render Markdown, code and diffs for the terminal

A new harness-tui crate draws the conversation with ratatui: Markdown
through pulldown-cmark, code blocks highlighted by syntect, and unified
diffs through similar, all sanitized and wrapped to the screen's width.
NO_COLOR turns every colour off.

<trailer lines from the controller>
EOF
```

---

### Task 3: The inline terminal and the transcript

**Files:**
- Modify: `crates/harness-tui/Cargo.toml`, `crates/harness-tui/src/lib.rs`
- Create: `crates/harness-tui/src/inline.rs`, `crates/harness-tui/src/transcript.rs`
- Test: `crates/harness-tui/tests/inline.rs`

**Interfaces:**
- Consumes: Task 2's `markdown::render`, `diff::unified`, `text::{lines, sanitize, wrap}`, `style::Theme`.
- Produces:
  - `inline::InlineTerminal<B: Backend>`: `new(backend, top: u16) -> io::Result<Self>` (`top` is the row the cursor was on), `insert(&[Line])` (lines above the live region, into scrollback as they scroll off), `draw(height, render: impl FnOnce(Rect, &mut Buffer) -> Option<Position>)` (only changed cells written; the cursor where `render` says, hidden for `None`), `clear()`, `resized()`, `width()`, `height()`, `top()`, `backend()`, `backend_mut()`;
  - `transcript::Transcript`: `new(Theme)`, `on_event(&AgentEvent, width)`, `take_finished() -> Vec<Line>`, `live(width, rows) -> Vec<Line>` (the reply streaming in, the running tool), `push_user`, `push_note`, `push_warning`, `push_error`, `push_lines`, `busy()`, `theme()`;
  - `transcript::call_summary(name, &Value) -> String` (`$ cmd`, `read path`, `edit path`, …).

- [ ] **Step 1: Write the failing tests**

In `crates/harness-tui/Cargo.toml`:

Replace:

```toml
publish.workspace = true

[dependencies]
pulldown-cmark.workspace = true
ratatui.workspace = true
similar.workspace = true
syntect.workspace = true
unicode-width.workspace = true
```

with:

```toml
publish.workspace = true

[dependencies]
harness-core.workspace = true
pulldown-cmark.workspace = true
ratatui.workspace = true
serde_json.workspace = true
similar.workspace = true
syntect.workspace = true
unicode-width.workspace = true
```

Create `crates/harness-tui/tests/inline.rs`:

```rust
//! The inline terminal and the transcript, on ratatui's `TestBackend`: finished lines go into the
//! terminal's scrollback, and only the live region is redrawn.

use std::sync::{Arc, Mutex};

use harness_core::event::{AgentEvent, TurnEndReason};
use harness_tui::{inline::InlineTerminal, style::Theme, transcript::Transcript};
use ratatui::{
    backend::{Backend, ClearType, TestBackend, WindowSize},
    buffer::{Buffer, Cell},
    layout::{Position, Size},
    text::Line,
    widgets::{Paragraph, Widget},
};

/// Each row of `buffer` as text, without trailing spaces.
fn rows(buffer: &Buffer) -> Vec<String> {
    let width = buffer.area.width as usize;
    buffer
        .content
        .chunks(width.max(1))
        .map(|row| {
            let text: String = row.iter().map(|cell| cell.symbol()).collect();
            text.trim_end().to_string()
        })
        .collect()
}

fn text_rows(lines: &[Line<'_>]) -> Vec<String> {
    lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

/// Draws `text` as the live region, `height` rows tall, with the cursor after it.
fn draw_live(
    term: &mut InlineTerminal<impl Backend<Error: Send + Sync + 'static>>,
    text: &str,
    height: u16,
) {
    term.draw(height, |area, buf| {
        Paragraph::new(text.to_string()).render(area, buf);
        Some(Position::new(text.len() as u16, area.y))
    })
    .unwrap();
}

#[test]
fn earlier_lines_stay_reachable_in_the_terminals_scrollback() {
    let mut term = InlineTerminal::new(TestBackend::new(20, 5), 0).unwrap();
    for i in 1..=8 {
        term.insert(&[Line::from(format!("line {i}"))]).unwrap();
        draw_live(&mut term, "> ", 1);
    }
    let backend = term.backend();
    assert_eq!(
        rows(backend.scrollback()),
        ["line 1", "line 2", "line 3", "line 4"]
    );
    assert_eq!(
        rows(backend.buffer()),
        ["line 5", "line 6", "line 7", "line 8", ">"]
    );
    assert_eq!(backend.cursor_position(), Position::new(2, 4));
}

#[test]
fn the_live_region_grows_and_shrinks_below_the_last_line() {
    let mut term = InlineTerminal::new(TestBackend::new(10, 5), 0).unwrap();
    term.insert(&[Line::from("one"), Line::from("two")])
        .unwrap();
    draw_live(&mut term, "a", 1);
    term.draw(3, |area, buf| {
        Paragraph::new("a\nb\nc").render(area, buf);
        None
    })
    .unwrap();
    assert_eq!(rows(term.backend().buffer()), ["one", "two", "a", "b", "c"]);
    draw_live(&mut term, "a", 1);
    assert_eq!(rows(term.backend().buffer()), ["one", "two", "a", "", ""]);
    term.backend().assert_scrollback_empty();
    // Taller than the room left: the lines above scroll into scrollback.
    term.draw(5, |area, buf| {
        Paragraph::new("1\n2\n3\n4\n5").render(area, buf);
        None
    })
    .unwrap();
    assert_eq!(rows(term.backend().scrollback()), ["one", "two"]);
    assert_eq!(rows(term.backend().buffer()), ["1", "2", "3", "4", "5"]);
    assert_eq!(term.top(), 0);
}

#[test]
fn clearing_leaves_the_finished_lines_and_the_cursor_below_them() {
    let mut term = InlineTerminal::new(TestBackend::new(10, 5), 1).unwrap();
    term.insert(&[Line::from("done")]).unwrap();
    draw_live(&mut term, "> typing", 2);
    term.clear().unwrap();
    assert_eq!(rows(term.backend().buffer()), ["", "done", "", "", ""]);
    assert_eq!(term.backend().cursor_position(), Position::new(0, 2));
}

#[test]
fn after_a_resize_the_live_region_stays_on_screen() {
    let mut term = InlineTerminal::new(TestBackend::new(20, 10), 8).unwrap();
    draw_live(&mut term, "> ", 2);
    term.backend_mut().resize(20, 5);
    term.resized().unwrap();
    draw_live(&mut term, "> ", 2);
    assert_eq!(term.top(), 3);
    assert_eq!(term.backend().cursor_position(), Position::new(2, 3));
}

/// A `TestBackend` that records the rows of every cell it is asked to draw.
struct Recording {
    inner: TestBackend,
    drawn: Arc<Mutex<Vec<(u16, u16)>>>,
}

impl Backend for Recording {
    type Error = core::convert::Infallible;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let cells: Vec<_> = content.collect();
        self.drawn
            .lock()
            .unwrap()
            .extend(cells.iter().map(|(x, y, _)| (*x, *y)));
        self.inner.draw(cells.into_iter())
    }
    fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
        self.inner.append_lines(n)
    }
    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()
    }
    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }
    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.inner.set_cursor_position(position)
    }
    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }
    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }
    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.size()
    }
    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }
    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }
}

#[test]
fn a_redraw_writes_only_the_live_cells_that_changed() {
    let drawn = Arc::new(Mutex::new(Vec::new()));
    let backend = Recording {
        inner: TestBackend::new(20, 6),
        drawn: drawn.clone(),
    };
    let mut term = InlineTerminal::new(backend, 0).unwrap();
    term.insert(&[Line::from("history")]).unwrap();
    draw_live(&mut term, "> hello", 1);
    drawn.lock().unwrap().clear();
    draw_live(&mut term, "> hellO", 1);
    assert_eq!(*drawn.lock().unwrap(), [(6, 1)]);
    // Nothing above the live region is touched by redraws.
    drawn.lock().unwrap().clear();
    draw_live(&mut term, "> other text", 1);
    assert!(drawn.lock().unwrap().iter().all(|(_, y)| *y == 1));
}

fn event_lines(events: &[AgentEvent], width: usize) -> (Transcript, Vec<String>) {
    let mut transcript = Transcript::new(Theme::monochrome());
    for event in events {
        transcript.on_event(event, width);
    }
    let finished = transcript.take_finished();
    let text = text_rows(&finished);
    (transcript, text)
}

#[test]
fn a_tool_using_turn_becomes_lines_for_the_scrollback() {
    let mut transcript = Transcript::new(Theme::monochrome());
    transcript.push_user("run the tests", 40);
    let events = [
        AgentEvent::TurnStarted,
        AgentEvent::ToolCallRequested {
            id: "c1".into(),
            name: "bash".into(),
            arguments: r#"{"command":"cargo test"}"#.into(),
        },
        AgentEvent::ToolCallFinished {
            id: "c1".into(),
            output: "exit code 0\nok\n".into(),
            is_error: false,
        },
        AgentEvent::TextDelta {
            text: "All **tests**".into(),
        },
    ];
    for event in &events {
        transcript.on_event(event, 40);
    }
    assert!(transcript.busy());
    // The reply streams in the live region until it is complete.
    assert_eq!(text_rows(&transcript.live(40, 5)), ["All tests"]);
    for event in [
        AgentEvent::AssistantMessage {
            content: "All **tests** pass.".into(),
            model: "mock/m".into(),
        },
        AgentEvent::TurnFinished {
            reason: TurnEndReason::Completed,
        },
    ] {
        transcript.on_event(&event, 40);
    }
    assert!(!transcript.busy());
    assert!(transcript.live(40, 5).is_empty());
    assert_eq!(
        text_rows(&transcript.take_finished()),
        [
            "› run the tests",
            "",
            "● $ cargo test",
            "  exit code 0",
            "  ok",
            "",
            "All tests pass.",
        ]
    );
}

#[test]
fn an_edit_shows_what_it_replaced_and_long_output_is_cut_short() {
    let output: String = (1..=10).map(|i| format!("line {i}\n")).collect();
    let (_, lines) = event_lines(
        &[
            AgentEvent::ToolCallRequested {
                id: "e".into(),
                name: "edit".into(),
                arguments:
                    r#"{"path":"src/a.rs","old_string":"let x = 1;","new_string":"let x = 2;"}"#
                        .into(),
            },
            AgentEvent::ToolCallFinished {
                id: "e".into(),
                output: "edited src/a.rs".into(),
                is_error: false,
            },
            AgentEvent::ToolCallRequested {
                id: "b".into(),
                name: "bash".into(),
                arguments: r#"{"command":"seq 10"}"#.into(),
            },
            AgentEvent::ToolCallFinished {
                id: "b".into(),
                output,
                is_error: false,
            },
        ],
        40,
    );
    assert_eq!(
        lines,
        [
            "● edit src/a.rs",
            "  -let x = 1;",
            "  +let x = 2;",
            "",
            "● $ seq 10",
            "  line 1",
            "  line 2",
            "  line 3",
            "  line 4",
            "  line 5",
            "  line 6",
            "  … 4 more lines",
        ]
    );
}

#[test]
fn the_live_region_shows_the_tail_of_a_long_reply_and_the_running_tool() {
    let mut transcript = Transcript::new(Theme::monochrome());
    transcript.on_event(
        &AgentEvent::TextDelta {
            text: "one\n\ntwo\n\nthree\n\nfour".into(),
        },
        20,
    );
    assert_eq!(text_rows(&transcript.live(20, 3)), ["three", "", "four"]);
    transcript.on_event(
        &AgentEvent::ToolCallRequested {
            id: "c".into(),
            name: "read".into(),
            arguments: r#"{"path":"README.md"}"#.into(),
        },
        20,
    );
    assert_eq!(
        text_rows(&transcript.live(20, 2)),
        ["four", "● read README.md"]
    );
}

#[test]
fn interrupted_and_failed_turns_say_so_and_keep_partial_output() {
    let (_, lines) = event_lines(
        &[
            AgentEvent::TurnStarted,
            AgentEvent::TextDelta {
                text: "partial".into(),
            },
            AgentEvent::TurnFinished {
                reason: TurnEndReason::Interrupted,
            },
        ],
        30,
    );
    assert_eq!(lines, ["partial", "interrupted"]);
    let (_, lines) = event_lines(
        &[
            AgentEvent::Error {
                kind: harness_core::event::ErrorKind::Provider,
                message: "HTTP 500: \u{1b}[31mboom".into(),
            },
            AgentEvent::TurnFinished {
                reason: TurnEndReason::StepLimit,
            },
        ],
        60,
    );
    assert_eq!(
        lines,
        [
            "error: HTTP 500: \\u{1b}[31mboom",
            "error: stopped after reaching the step limit",
        ]
    );
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p harness-tui --test inline`
Expected: FAIL to compile: ``unresolved imports `harness_tui::inline`, `harness_tui::transcript` ``.

- [ ] **Step 3: The inline terminal**

Create `crates/harness-tui/src/inline.rs`:

```rust
//! The terminal, drawn inline: finished lines are written above a live region at the bottom of
//! what harness has drawn, and scroll off into the terminal's own scrollback, where the terminal's
//! scrolling, search and copy work as usual. Only the live region is redrawn, and only the cells
//! in it that changed. Unlike ratatui's inline viewport, the live region's height changes with
//! what it shows.

use std::io;

use ratatui::{
    backend::{Backend, ClearType},
    buffer::Buffer,
    layout::{Position, Rect, Size},
    text::Line,
    widgets::Widget,
};

/// A terminal whose bottom rows, from `top` down, are a live region that is redrawn; everything
/// above it is written once.
pub struct InlineTerminal<B: Backend> {
    backend: B,
    screen: Size,
    /// The live region's first row.
    top: u16,
    height: u16,
    /// What the live region shows, so a redraw writes only what changed.
    shown: Buffer,
}

fn io_error<E: std::error::Error + Send + Sync + 'static>(error: E) -> io::Error {
    io::Error::other(error)
}

impl<B> InlineTerminal<B>
where
    B: Backend,
    B::Error: Send + Sync + 'static,
{
    /// Starts drawing at row `top`, where the cursor was when harness started: the rows above it
    /// are the user's.
    pub fn new(backend: B, top: u16) -> io::Result<Self> {
        let screen = backend.size().map_err(io_error)?;
        let top = top.min(screen.height.saturating_sub(1));
        Ok(InlineTerminal {
            backend,
            screen,
            top,
            height: 0,
            shown: Buffer::empty(Rect::new(0, top, screen.width, 0)),
        })
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    /// The screen's width in columns.
    pub fn width(&self) -> u16 {
        self.screen.width
    }

    /// The screen's height in rows.
    pub fn height(&self) -> u16 {
        self.screen.height
    }

    /// The live region's first row.
    pub fn top(&self) -> u16 {
        self.top
    }

    /// Scrolls the whole screen up by `rows`, the top rows going into scrollback.
    fn scroll_up(&mut self, rows: u16) -> io::Result<()> {
        if rows == 0 {
            return Ok(());
        }
        let bottom = self.screen.height.saturating_sub(1);
        self.backend
            .set_cursor_position(Position::new(0, bottom))
            .map_err(io_error)?;
        self.backend.append_lines(rows).map_err(io_error)
    }

    /// Clears from the live region's top to the bottom of the screen, and forgets what it showed.
    fn clear_live(&mut self) -> io::Result<()> {
        self.backend
            .set_cursor_position(Position::new(0, self.top))
            .map_err(io_error)?;
        self.backend
            .clear_region(ClearType::AfterCursor)
            .map_err(io_error)?;
        self.shown = Buffer::empty(Rect::new(0, self.top, self.screen.width, self.height));
        Ok(())
    }

    /// Writes `lines`, each at most the screen's width, above the live region, which moves down
    /// (and, at the bottom of the screen, pushes the rows above into scrollback). The live region
    /// is cleared: draw it again afterwards.
    pub fn insert(&mut self, lines: &[Line<'_>]) -> io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let height = self.height;
        self.height = 0;
        self.clear_live()?;
        let width = self.screen.width;
        let mut y = self.top;
        for line in lines {
            if y >= self.screen.height {
                self.scroll_up(1)?;
                y = self.screen.height - 1;
            }
            let area = Rect::new(0, y, width, 1);
            let mut row = Buffer::empty(area);
            line.render(area, &mut row);
            self.backend
                .draw(
                    row.content
                        .iter()
                        .enumerate()
                        .map(|(x, cell)| (x as u16, y, cell)),
                )
                .map_err(io_error)?;
            y += 1;
        }
        self.top = y.min(self.screen.height);
        self.make_room(height)?;
        self.height = height.min(self.screen.height);
        self.shown = Buffer::empty(Rect::new(0, self.top, width, self.height));
        self.backend.flush().map_err(io_error)
    }

    /// Moves the live region up far enough for `height` rows below its top.
    fn make_room(&mut self, height: u16) -> io::Result<()> {
        let height = height.min(self.screen.height);
        let over = (self.top + height).saturating_sub(self.screen.height);
        self.scroll_up(over)?;
        self.top -= over;
        Ok(())
    }

    /// Draws the live region, `height` rows tall, with `render`, which returns where the cursor
    /// goes (it is hidden for `None`). Only cells that changed since the last draw are written.
    pub fn draw(
        &mut self,
        height: u16,
        render: impl FnOnce(Rect, &mut Buffer) -> Option<Position>,
    ) -> io::Result<()> {
        let height = height.min(self.screen.height);
        if height != self.height {
            self.make_room(height)?;
            self.height = height;
            self.clear_live()?;
        }
        let area = Rect::new(0, self.top, self.screen.width, height);
        let mut next = Buffer::empty(area);
        let cursor = render(area, &mut next);
        let updates = self.shown.diff(&next);
        self.backend.draw(updates.into_iter()).map_err(io_error)?;
        match cursor {
            Some(position) => {
                self.backend
                    .set_cursor_position(position)
                    .map_err(io_error)?;
                self.backend.show_cursor().map_err(io_error)?;
            }
            None => self.backend.hide_cursor().map_err(io_error)?,
        }
        self.shown = next;
        self.backend.flush().map_err(io_error)
    }

    /// Clears the live region and leaves the cursor at its top, as harness exits or hands the
    /// terminal to another program.
    pub fn clear(&mut self) -> io::Result<()> {
        self.height = 0;
        self.clear_live()?;
        self.backend.show_cursor().map_err(io_error)?;
        self.backend.flush().map_err(io_error)
    }

    /// After the terminal changed size: the live region stays on screen and is drawn anew.
    pub fn resized(&mut self) -> io::Result<()> {
        self.screen = self.backend.size().map_err(io_error)?;
        let height = self.height.min(self.screen.height);
        self.top = self
            .top
            .min(self.screen.height.saturating_sub(height.max(1)));
        self.height = height;
        self.clear_live()
    }
}
```

- [ ] **Step 4: The transcript**

Create `crates/harness-tui/src/transcript.rs`:

```rust
//! The conversation as the user sees it: what the agent reports turns into lines for the
//! scrollback once each part is finished, and what is still streaming or running is shown in
//! the live region.

use std::collections::HashMap;

use harness_core::event::{AgentEvent, TurnEndReason};
use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::{
    diff, markdown,
    style::Theme,
    text::{lines, sanitize, wrap},
};

/// Lines of a tool's output shown in the transcript; the rest is summarized.
const OUTPUT_LINES: usize = 6;
/// Lines of an edit's diff shown in the transcript.
const DIFF_LINES: usize = 20;

/// A tool call that has not finished.
#[derive(Debug, Clone)]
struct Call {
    name: String,
    arguments: Value,
}

/// The conversation's finished lines, waiting to go into scrollback, and what is in progress.
pub struct Transcript {
    theme: Theme,
    /// Finished lines not yet written to the terminal.
    pending: Vec<Line<'static>>,
    /// The assistant's reply as it streams.
    streaming: String,
    /// The model is reasoning (its reasoning is not shown).
    thinking: bool,
    calls: HashMap<String, Call>,
    /// The call running now, for the live region.
    running: Option<String>,
    /// Whether a turn is running.
    busy: bool,
    /// Whether the last finished line is blank, or there is none yet.
    blank: bool,
}

impl Transcript {
    pub fn new(theme: Theme) -> Transcript {
        Transcript {
            theme,
            pending: Vec::new(),
            streaming: String::new(),
            thinking: false,
            calls: HashMap::new(),
            running: None,
            busy: false,
            blank: true,
        }
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Whether a turn is running.
    pub fn busy(&self) -> bool {
        self.busy
    }

    /// The finished lines not yet written, which are then forgotten.
    pub fn take_finished(&mut self) -> Vec<Line<'static>> {
        std::mem::take(&mut self.pending)
    }

    /// Adds `lines` to the finished lines.
    fn emit(&mut self, lines: impl IntoIterator<Item = Line<'static>>) {
        for line in lines {
            self.blank = line.spans.iter().all(|s| s.content.trim().is_empty());
            self.pending.push(line);
        }
    }

    /// A blank line between one part of the conversation and the next.
    fn gap(&mut self) {
        if !self.blank {
            self.emit([Line::default()]);
        }
    }

    /// Adds `lines`, wrapped to `width`, to the finished lines.
    pub fn push_lines(&mut self, lines: Vec<Line<'static>>, width: usize) {
        for line in lines {
            self.emit(wrap(&line, width, &[], &[]));
        }
    }

    /// What the user sent.
    pub fn push_user(&mut self, text: &str, width: usize) {
        self.gap();
        let style = self.theme.user();
        let marker = Span::styled("› ", self.theme.accent());
        for (i, line) in lines(text, style).into_iter().enumerate() {
            let first = if i == 0 {
                vec![marker.clone()]
            } else {
                vec![Span::raw("  ")]
            };
            self.emit(wrap(&line, width, &first, &[Span::raw("  ")]));
        }
    }

    /// A note from harness, dim.
    pub fn push_note(&mut self, text: &str, width: usize) {
        let style = self.theme.dim();
        self.push_lines(lines(text, style), width);
    }

    pub fn push_warning(&mut self, text: &str, width: usize) {
        let style = self.theme.warning();
        self.push_lines(lines(&format!("warning: {text}"), style), width);
    }

    pub fn push_error(&mut self, text: &str, width: usize) {
        let style = self.theme.error();
        self.push_lines(lines(&format!("error: {text}"), style), width);
    }

    /// Takes in one event from the agent. `width` is the screen's width.
    pub fn on_event(&mut self, event: &AgentEvent, width: usize) {
        match event {
            AgentEvent::TurnStarted => {
                self.busy = true;
                self.streaming.clear();
            }
            AgentEvent::TextDelta { text } => {
                self.thinking = false;
                self.streaming.push_str(text);
            }
            AgentEvent::ReasoningDelta { .. } => self.thinking = true,
            AgentEvent::AssistantMessage { content, .. } => {
                self.thinking = false;
                self.streaming.clear();
                if !content.trim().is_empty() {
                    self.gap();
                    let rendered = markdown::render(content, width, &self.theme);
                    self.emit(rendered);
                }
            }
            AgentEvent::ToolCallRequested {
                id,
                name,
                arguments,
            } => {
                let arguments = serde_json::from_str(arguments).unwrap_or(Value::Null);
                self.calls.insert(
                    id.clone(),
                    Call {
                        name: name.clone(),
                        arguments,
                    },
                );
                self.running = Some(id.clone());
            }
            AgentEvent::ToolCallFinished {
                id,
                output,
                is_error,
            } => {
                if self.running.as_ref() == Some(id) {
                    self.running = None;
                }
                let call = self.calls.remove(id).unwrap_or(Call {
                    name: "tool".into(),
                    arguments: Value::Null,
                });
                self.finish_call(&call, output, *is_error, width);
            }
            AgentEvent::ActionBlocked { reason, .. } => {
                let style = self.theme.warning();
                self.push_lines(lines(&format!("blocked: {reason}"), style), width);
            }
            AgentEvent::Retrying {
                attempt,
                reason,
                delay_ms,
            } => {
                let text = format!(
                    "retrying (attempt {attempt}) in {:.1}s: {reason}",
                    *delay_ms as f64 / 1000.0
                );
                self.push_note(&text, width);
            }
            AgentEvent::Warning { message } => self.push_warning(message, width),
            AgentEvent::Error { message, .. } => self.push_error(message, width),
            AgentEvent::Compacted {
                summary,
                tokens_before,
                tokens_after,
            } => {
                self.gap();
                self.push_note(
                    &format!(
                        "compacted the conversation from about {tokens_before} to {tokens_after} tokens; summary:"
                    ),
                    width,
                );
                let rendered = markdown::render(summary, width, &self.theme);
                self.emit(rendered);
            }
            AgentEvent::TurnFinished { reason } => {
                self.busy = false;
                self.thinking = false;
                self.running = None;
                if !self.streaming.trim().is_empty() {
                    // Output that never became a message: keep what arrived.
                    let text = std::mem::take(&mut self.streaming);
                    self.gap();
                    let rendered = markdown::render(&text, width, &self.theme);
                    self.emit(rendered);
                }
                self.streaming.clear();
                match reason {
                    TurnEndReason::Interrupted => self.push_note("interrupted", width),
                    TurnEndReason::StepLimit => {
                        self.push_error("stopped after reaching the step limit", width)
                    }
                    TurnEndReason::Completed | TurnEndReason::Error => {}
                }
            }
            AgentEvent::ApprovalNeeded { .. }
            | AgentEvent::Usage { .. }
            | AgentEvent::CheckpointCreated { .. } => {}
        }
    }

    /// The lines for a finished tool call: what it did, then a short look at its result.
    fn finish_call(&mut self, call: &Call, output: &str, is_error: bool, width: usize) {
        self.gap();
        let header = call_summary(&call.name, &call.arguments);
        let marker = Span::styled("● ", self.theme.accent());
        let style = if is_error {
            self.theme.error()
        } else {
            self.theme.bold()
        };
        let line = Line::from(Span::styled(sanitize(&header).replace('\n', " "), style));
        self.emit(wrap(&line, width, &[marker], &[Span::raw("  ")]));
        let indent = [Span::raw("  ")];
        let body: Vec<Line<'static>> = match call.name.as_str() {
            "edit" if !is_error => {
                let old = call.arguments["old_string"].as_str().unwrap_or_default();
                let new = call.arguments["new_string"].as_str().unwrap_or_default();
                let mut diff = diff::unified(old, new, 3, &self.theme);
                // The hunk header's line numbers are the snippet's, not the file's.
                diff.retain(|l| !l.spans.iter().any(|s| s.content.starts_with("@@")));
                clip(diff, DIFF_LINES, &self.theme)
            }
            "read" | "write" if !is_error => Vec::new(),
            _ => clip(
                lines(output.trim_end(), self.theme.dim()),
                OUTPUT_LINES,
                &self.theme,
            ),
        };
        for line in body {
            self.emit(wrap(&line, width, &indent, &indent));
        }
    }

    /// What the live region shows of the turn in progress, at most `rows` lines: the tail of the
    /// reply streaming in, and what is running.
    pub fn live(&self, width: usize, rows: usize) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        if !self.streaming.is_empty() {
            out = markdown::render(&self.streaming, width, &self.theme);
        }
        if let Some(call) = self.running.as_ref().and_then(|id| self.calls.get(id)) {
            let summary = call_summary(&call.name, &call.arguments);
            let line = Line::from(vec![
                Span::styled("● ", self.theme.accent()),
                Span::styled(sanitize(&summary).replace('\n', " "), self.theme.dim()),
            ]);
            out.extend(wrap(&line, width, &[], &[Span::raw("  ")]));
        } else if self.thinking {
            out.push(Line::from(Span::styled("thinking…", self.theme.dim())));
        }
        let skip = out.len().saturating_sub(rows);
        out.split_off(skip)
    }
}

/// At most `max` of `lines`, then a line saying how many more there are.
fn clip(mut lines: Vec<Line<'static>>, max: usize, theme: &Theme) -> Vec<Line<'static>> {
    if lines.len() > max {
        let more = lines.len() - max;
        lines.truncate(max);
        lines.push(Line::from(Span::styled(
            format!("… {more} more line{}", if more == 1 { "" } else { "s" }),
            theme.dim(),
        )));
    }
    lines
}

/// One line saying what a tool call does, from its arguments.
pub fn call_summary(name: &str, args: &Value) -> String {
    let text = |key: &str| args[key].as_str().unwrap_or_default().to_string();
    match name {
        "bash" => format!("$ {}", text("command").lines().next().unwrap_or_default()),
        "read" => format!("read {}", text("path")),
        "write" => {
            let lines = args["content"].as_str().map_or(0, |c| c.lines().count());
            format!("write {} ({lines} lines)", text("path"))
        }
        "edit" => format!("edit {}", text("path")),
        "grep" => match args["path"].as_str() {
            Some(path) => format!("grep {} in {path}", text("pattern")),
            None => format!("grep {}", text("pattern")),
        },
        "glob" => format!("glob {}", text("pattern")),
        other => {
            let shown: String = args.to_string().chars().take(80).collect();
            format!("{other} {shown}")
        }
    }
}
```

In `crates/harness-tui/src/lib.rs`:

Replace:

```rust

pub mod diff;
pub mod highlight;
pub mod markdown;
pub mod style;
pub mod text;
```

with:

```rust

pub mod diff;
pub mod highlight;
pub mod inline;
pub mod markdown;
pub mod style;
pub mod text;
pub mod transcript;
```

- [ ] **Step 5: Run the tests, and lint**

Run: `cargo test -p harness-tui`
Expected: PASS, including `earlier_lines_stay_reachable_in_the_terminals_scrollback` and `a_redraw_writes_only_the_live_cells_that_changed`.

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add Cargo.lock crates/harness-tui
git commit -F - <<'EOF'
feat(tui): draw inline, with finished lines in the scrollback

InlineTerminal keeps a live region at the bottom of what harness has
drawn and redraws only the cells in it that changed; finished lines are
written above it and scroll into the terminal's own scrollback. The
transcript turns the agent's events into those lines, and shows the
reply streaming in and the tool running in the live region.

<trailer lines from the controller>
EOF
```

---

### Task 4: The input editor

**Files:**
- Modify: `crates/harness-tui/src/lib.rs`
- Create: `crates/harness-tui/src/editor.rs`
- Test: `crates/harness-tui/tests/editor.rs`

**Interfaces:**
- Consumes: Task 2's `text::sanitize`, `text::width`, `style::Theme`.
- Produces: `editor::Editor`: `new(history: Vec<String>)`, `key(KeyEvent) -> Edit` (`Handled`, `Submit`, `Ignored`), `paste(&str)` (collapsed over `PASTE_MAX_LINES` = 10 lines or `PASTE_MAX_CHARS` = 1,000 characters), `insert(&str)`, `expand_paste() -> bool`, `submit() -> (shown, full)`, `text()`, `expanded()`, `cursor()`, `is_empty()`, `set_text(&str)`, `clear()`, `remember(&str)`, and `render(prompt, width, &Theme) -> (Vec<Line>, Position)`.

- [ ] **Step 1: Write the failing tests**

Create `crates/harness-tui/tests/editor.rs`:

```rust
//! The input editor, driven by scripted keys and pastes.

use harness_tui::{
    editor::{Edit, Editor},
    style::Theme,
    text::plain,
};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    layout::Position,
};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn with(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

fn ctrl(c: char) -> KeyEvent {
    with(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn type_text(editor: &mut Editor, text: &str) {
    for c in text.chars() {
        assert_eq!(editor.key(key(KeyCode::Char(c))), Edit::Handled);
    }
}

#[test]
fn typing_editing_and_moving_the_cursor() {
    let mut editor = Editor::new(Vec::new());
    type_text(&mut editor, "fix the bug");
    editor.key(ctrl('w'));
    assert_eq!(editor.text(), "fix the ");
    type_text(&mut editor, "tests");
    editor.key(key(KeyCode::Home));
    editor.key(key(KeyCode::Delete));
    type_text(&mut editor, "F");
    assert_eq!(editor.text(), "Fix the tests");
    editor.key(ctrl('e'));
    editor.key(key(KeyCode::Backspace));
    assert_eq!(editor.text(), "Fix the test");
    editor.key(with(KeyCode::Left, KeyModifiers::ALT));
    assert_eq!(editor.cursor(), "Fix the ".len());
    editor.key(ctrl('k'));
    assert_eq!(editor.text(), "Fix the ");
    assert_eq!(editor.key(key(KeyCode::Enter)), Edit::Submit);
    assert_eq!(editor.key(key(KeyCode::Esc)), Edit::Ignored);
}

#[test]
fn alt_enter_shift_enter_ctrl_j_and_a_trailing_backslash_insert_newlines() {
    let mut editor = Editor::new(Vec::new());
    type_text(&mut editor, "a");
    editor.key(with(KeyCode::Enter, KeyModifiers::ALT));
    type_text(&mut editor, "b");
    editor.key(with(KeyCode::Enter, KeyModifiers::SHIFT));
    type_text(&mut editor, "c");
    editor.key(ctrl('j'));
    type_text(&mut editor, "d\\");
    assert_eq!(editor.key(key(KeyCode::Enter)), Edit::Handled);
    type_text(&mut editor, "e");
    assert_eq!(editor.text(), "a\nb\nc\nd\ne");
    // Up and Down move between rows before they recall history.
    editor.key(key(KeyCode::Up));
    editor.key(key(KeyCode::Up));
    type_text(&mut editor, "C");
    assert_eq!(editor.text(), "a\nb\ncC\nd\ne");
    assert_eq!(
        editor.submit(),
        ("a\nb\ncC\nd\ne".to_string(), "a\nb\ncC\nd\ne".to_string())
    );
    assert!(editor.is_empty());
}

#[test]
fn pasting_a_stack_trace_shows_a_placeholder_and_sends_every_line() {
    let trace: String = (1..=200).map(|i| format!("at frame {i}\r\n")).collect();
    let mut editor = Editor::new(Vec::new());
    type_text(&mut editor, "why does this fail? ");
    editor.paste(&trace);
    type_text(&mut editor, " thanks");
    assert_eq!(
        editor.text(),
        "why does this fail? [Pasted text #1, 200 lines] thanks"
    );
    let (shown, full) = editor.submit();
    assert_eq!(
        shown,
        "why does this fail? [Pasted text #1, 200 lines] thanks"
    );
    assert_eq!(full.lines().filter(|l| l.contains("at frame")).count(), 200);
    assert!(full.starts_with("why does this fail? at frame 1\nat frame 2\n"));
    assert!(!full.contains('\r'));
    // Short pastes are typed in as they are; long single lines collapse too.
    editor.paste("short\npaste");
    assert_eq!(editor.text(), "short\npaste");
    editor.clear();
    editor.paste(&"x".repeat(1_001));
    assert_eq!(editor.text(), "[Pasted text #2, 1 line]");
}

#[test]
fn a_placeholder_is_deleted_whole_and_can_be_expanded_for_editing() {
    let text: String = (1..=12).map(|i| format!("{i}\n")).collect();
    let mut editor = Editor::new(Vec::new());
    editor.paste(&text);
    type_text(&mut editor, "!");
    // The cursor skips over a placeholder as a unit.
    editor.key(key(KeyCode::Left));
    editor.key(key(KeyCode::Left));
    assert_eq!(editor.cursor(), 0);
    editor.key(key(KeyCode::Right));
    assert_eq!(editor.cursor(), "[Pasted text #1, 12 lines]".len());
    assert!(editor.expand_paste());
    assert_eq!(editor.text(), format!("{text}!"));
    assert_eq!(editor.expanded(), format!("{text}!"));
    editor.clear();
    editor.paste(&text);
    editor.key(key(KeyCode::Backspace));
    assert_eq!(editor.text(), "");
    assert_eq!(editor.expanded(), "");
    // Ctrl+O expands too.
    editor.paste(&text);
    assert_eq!(editor.key(ctrl('o')), Edit::Handled);
    assert_eq!(editor.text(), text);
}

#[test]
fn up_and_down_recall_earlier_inputs_and_the_draft() {
    let mut editor = Editor::new(vec!["from the resumed session".to_string()]);
    type_text(&mut editor, "first");
    editor.submit();
    let long: String = (1..=20).map(|i| format!("{i}\n")).collect();
    editor.paste(&long);
    editor.submit();
    type_text(&mut editor, "draft");
    editor.key(key(KeyCode::Up));
    // A long entry comes back collapsed, and is sent in full.
    assert!(
        editor.text().starts_with("[Pasted text #"),
        "{}",
        editor.text()
    );
    assert_eq!(editor.expanded(), long);
    editor.key(key(KeyCode::Up));
    assert_eq!(editor.text(), "first");
    editor.key(key(KeyCode::Up));
    assert_eq!(editor.text(), "from the resumed session");
    assert_eq!(editor.key(key(KeyCode::Up)), Edit::Ignored);
    editor.key(key(KeyCode::Down));
    editor.key(key(KeyCode::Down));
    editor.key(key(KeyCode::Down));
    assert_eq!(editor.text(), "draft");
    assert_eq!(editor.key(key(KeyCode::Down)), Edit::Ignored);
}

#[test]
fn the_editor_wraps_long_lines_and_places_the_cursor() {
    let mut editor = Editor::new(Vec::new());
    type_text(&mut editor, "abcdefghij");
    editor.key(with(KeyCode::Enter, KeyModifiers::ALT));
    type_text(&mut editor, "xy");
    let (lines, cursor) = editor.render("› ", 8, &Theme::monochrome());
    let text: Vec<String> = lines.iter().map(plain).collect();
    assert_eq!(text, ["› abcdef", "  ghij", "  xy"]);
    assert_eq!(cursor, Position::new(4, 2));
    editor.key(key(KeyCode::Home));
    let (_, cursor) = editor.render("› ", 8, &Theme::monochrome());
    assert_eq!(cursor, Position::new(2, 2));
    // A cursor at the end of a full row goes to the next one.
    let mut editor = Editor::new(Vec::new());
    type_text(&mut editor, "abcdef");
    let (lines, cursor) = editor.render("› ", 8, &Theme::monochrome());
    assert_eq!(lines.len(), 2);
    assert_eq!(cursor, Position::new(2, 1));
    // Control characters typed or pasted are shown escaped.
    let mut editor = Editor::new(Vec::new());
    editor.paste("a\u{1b}[2Jb");
    let (lines, _) = editor.render("› ", 40, &Theme::monochrome());
    assert_eq!(plain(&lines[0]), "› a\\u{1b}[2Jb");
}

// Review Focus: wide characters (CJK, emoji) take two columns, so the cursor and wrapping must
// count columns, not characters.
#[test]
fn wide_characters_take_two_columns_for_the_cursor_and_wrapping() {
    let mut editor = Editor::new(Vec::new());
    type_text(&mut editor, "日本語 ok");
    let (lines, cursor) = editor.render("› ", 40, &Theme::monochrome());
    assert_eq!(plain(&lines[0]), "› 日本語 ok");
    assert_eq!(cursor, Position::new(2 + 6 + 3, 0));
    editor.key(key(KeyCode::Home));
    editor.key(key(KeyCode::Right));
    let (_, cursor) = editor.render("› ", 40, &Theme::monochrome());
    assert_eq!(cursor, Position::new(4, 0));
    // A wide character never straddles the edge: it moves to the next row.
    let mut editor = Editor::new(Vec::new());
    type_text(&mut editor, "abc🙂");
    let (lines, cursor) = editor.render("› ", 6, &Theme::monochrome());
    let text: Vec<String> = lines.iter().map(plain).collect();
    assert_eq!(text, ["› abc", "  🙂"]);
    assert_eq!(cursor, Position::new(4, 1));
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p harness-tui --test editor`
Expected: FAIL to compile: ``unresolved import `harness_tui::editor` ``.

- [ ] **Step 3: The editor**

Create `crates/harness-tui/src/editor.rs`:

```rust
//! The input editor: multi-line text with a cursor, recall of earlier inputs, and large pastes
//! collapsed to a placeholder whose full text is sent with the message.

use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::Position,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthChar;

use crate::{style::Theme, text::sanitize};

/// A paste with more lines than this is collapsed.
pub const PASTE_MAX_LINES: usize = 10;
/// A paste with more characters than this is collapsed.
pub const PASTE_MAX_CHARS: usize = 1_000;

/// A collapsed paste.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Paste {
    /// Where its placeholder is in the text, in bytes.
    start: usize,
    end: usize,
    text: String,
}

/// What a key did to the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    /// The key changed the text or the cursor.
    Handled,
    /// Enter: the input is ready to send.
    Submit,
    /// Not an editing key.
    Ignored,
}

/// The text being typed.
#[derive(Debug, Clone, Default)]
pub struct Editor {
    text: String,
    /// A byte offset into `text`, always at a character boundary and never inside a placeholder.
    cursor: usize,
    pastes: Vec<Paste>,
    /// Pastes collapsed so far, for their numbers.
    pasted: usize,
    /// Earlier inputs, oldest first.
    history: Vec<String>,
    /// While recalling: the entry shown, and the text that was being typed before.
    recall: Option<(usize, String)>,
}

impl Editor {
    /// An empty editor that recalls `history` (oldest first) with the Up key.
    pub fn new(history: Vec<String>) -> Editor {
        Editor {
            history,
            ..Editor::default()
        }
    }

    /// The text as shown, placeholders included.
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The cursor, as a byte offset into [`text`](Self::text).
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The text with every placeholder replaced by what was pasted.
    pub fn expanded(&self) -> String {
        let mut out = String::new();
        let mut at = 0;
        for paste in &self.pastes {
            out.push_str(&self.text[at..paste.start]);
            out.push_str(&paste.text);
            at = paste.end;
        }
        out.push_str(&self.text[at..]);
        out
    }

    /// Replaces the text, with the cursor at its end.
    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = self.text.len();
        self.pastes.clear();
        self.recall = None;
    }

    /// Empties the editor.
    pub fn clear(&mut self) {
        self.set_text("");
    }

    /// Takes the input to send: the text as shown, and in full. It is added to the history.
    pub fn submit(&mut self) -> (String, String) {
        let shown = self.text.clone();
        let full = self.expanded();
        if !full.trim().is_empty() && self.history.last() != Some(&full) {
            self.history.push(full.clone());
        }
        self.clear();
        (shown, full)
    }

    /// Adds `text` to the history without sending it (input sent some other way).
    pub fn remember(&mut self, text: &str) {
        if !text.trim().is_empty() && self.history.last().map(String::as_str) != Some(text) {
            self.history.push(text.to_string());
        }
    }

    /// The paste whose placeholder contains `offset` strictly inside, or ends at it.
    fn paste_ending_at(&self, offset: usize) -> Option<usize> {
        self.pastes
            .iter()
            .position(|p| p.start < offset && offset <= p.end)
    }

    fn paste_starting_at(&self, offset: usize) -> Option<usize> {
        self.pastes
            .iter()
            .position(|p| p.start <= offset && offset < p.end)
    }

    /// Replaces `start..end` of the text with `with`, moving the pastes after it.
    fn splice(&mut self, start: usize, end: usize, with: &str) {
        self.text.replace_range(start..end, with);
        let delta = with.len() as isize - (end - start) as isize;
        self.pastes.retain(|p| p.end <= start || p.start >= end);
        for paste in &mut self.pastes {
            if paste.start >= end {
                paste.start = (paste.start as isize + delta) as usize;
                paste.end = (paste.end as isize + delta) as usize;
            }
        }
        self.recall = None;
    }

    /// Types `text` at the cursor.
    pub fn insert(&mut self, text: &str) {
        let at = self.cursor;
        self.splice(at, at, text);
        self.cursor = at + text.len();
    }

    /// Pastes `text`: collapsed to `[Pasted text #n, N lines]` when it has more than
    /// [`PASTE_MAX_LINES`] lines or [`PASTE_MAX_CHARS`] characters, typed in as it is otherwise.
    pub fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let lines = text.lines().count();
        if lines <= PASTE_MAX_LINES && text.chars().count() <= PASTE_MAX_CHARS {
            self.insert(&text);
            return;
        }
        self.pasted += 1;
        let placeholder = format!(
            "[Pasted text #{}, {lines} line{}]",
            self.pasted,
            if lines == 1 { "" } else { "s" }
        );
        let at = self.cursor;
        self.splice(at, at, &placeholder);
        self.pastes.push(Paste {
            start: at,
            end: at + placeholder.len(),
            text,
        });
        self.pastes.sort_by_key(|p| p.start);
        self.cursor = at + placeholder.len();
    }

    /// Replaces the placeholder at the cursor (or the last one before it) with what was pasted,
    /// so it can be edited. Returns whether there was one.
    pub fn expand_paste(&mut self) -> bool {
        let index = self
            .paste_ending_at(self.cursor)
            .or_else(|| self.paste_starting_at(self.cursor))
            .or_else(|| self.pastes.iter().rposition(|p| p.end <= self.cursor));
        let Some(index) = index else {
            return false;
        };
        let paste = self.pastes.remove(index);
        let (start, end) = (paste.start, paste.end);
        self.splice(start, end, &paste.text);
        self.cursor = start + paste.text.len();
        true
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        if let Some(i) = self.paste_ending_at(offset) {
            return self.pastes[i].start;
        }
        self.text[..offset]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        if let Some(i) = self.paste_starting_at(offset) {
            return self.pastes[i].end;
        }
        self.text[offset..]
            .chars()
            .next()
            .map_or(offset, |c| offset + c.len_utf8())
    }

    fn line_start(&self, offset: usize) -> usize {
        self.text[..offset].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self, offset: usize) -> usize {
        self.text[offset..]
            .find('\n')
            .map_or(self.text.len(), |i| offset + i)
    }

    /// Where the word before `offset` starts.
    fn word_start(&self, offset: usize) -> usize {
        let before = &self.text[..offset];
        let trimmed = before.trim_end_matches(char::is_whitespace);
        trimmed.rfind(char::is_whitespace).map_or(0, |i| {
            i + trimmed[i..].chars().next().map_or(1, char::len_utf8)
        })
    }

    /// Where the word after `offset` ends.
    fn word_end(&self, offset: usize) -> usize {
        let after = &self.text[offset..];
        let skipped = after.len() - after.trim_start_matches(char::is_whitespace).len();
        let rest = &after[skipped..];
        offset + skipped + rest.find(char::is_whitespace).unwrap_or(rest.len())
    }

    /// A cursor position never inside a placeholder.
    fn settle(&self, offset: usize) -> usize {
        match self.paste_starting_at(offset) {
            Some(i) if self.pastes[i].start != offset => self.pastes[i].end,
            _ => offset,
        }
    }

    fn delete(&mut self, start: usize, end: usize) {
        let start =
            self.pastes.iter().fold(
                start,
                |s, p| {
                    if p.start < s && s < p.end { p.start } else { s }
                },
            );
        let end = self
            .pastes
            .iter()
            .fold(end, |e, p| if p.start < e && e < p.end { p.end } else { e });
        self.splice(start, end, "");
        self.cursor = start;
    }

    /// Moves the cursor a row up (`-1`) or down (`1`), keeping its column; `false` at the first
    /// or last row.
    fn move_row(&mut self, direction: isize) -> bool {
        let start = self.line_start(self.cursor);
        let column = self.text[start..self.cursor].chars().count();
        let target = if direction < 0 {
            if start == 0 {
                return false;
            }
            self.line_start(start - 1)
        } else {
            let end = self.line_end(self.cursor);
            if end == self.text.len() {
                return false;
            }
            end + 1
        };
        let end = self.line_end(target);
        let offset = self.text[target..end]
            .char_indices()
            .nth(column)
            .map_or(end, |(i, _)| target + i);
        self.cursor = self.settle(offset);
        true
    }

    /// Shows the previous (`-1`) or next (`1`) history entry.
    fn recall(&mut self, direction: isize) -> bool {
        let (index, draft) = match self.recall.take() {
            Some(state) => state,
            None if direction < 0 => (self.history.len(), self.expanded()),
            None => return false,
        };
        let next = index as isize + direction;
        if next < 0 {
            self.recall = Some((index, draft));
            return false;
        }
        let next = next as usize;
        let text = if next >= self.history.len() {
            draft.clone()
        } else {
            self.history[next].clone()
        };
        self.set_text("");
        self.paste(&text);
        if next < self.history.len() {
            self.recall = Some((next, draft));
        }
        true
    }

    /// Handles an editing key. Enter asks to submit, unless the character before the cursor is a
    /// backslash, which it turns into a new line.
    pub fn key(&mut self, key: KeyEvent) -> Edit {
        if key.kind == KeyEventKind::Release {
            return Edit::Ignored;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Enter if alt || shift => self.insert("\n"),
            KeyCode::Char('j') if ctrl => self.insert("\n"),
            KeyCode::Enter if self.text[..self.cursor].ends_with('\\') => {
                let at = self.cursor - 1;
                self.splice(at, at + 1, "\n");
            }
            KeyCode::Enter => return Edit::Submit,
            KeyCode::Char('a') if ctrl => self.cursor = self.line_start(self.cursor),
            KeyCode::Char('e') if ctrl => self.cursor = self.line_end(self.cursor),
            KeyCode::Char('b') if alt => self.cursor = self.settle(self.word_start(self.cursor)),
            KeyCode::Char('f') if alt => self.cursor = self.settle(self.word_end(self.cursor)),
            KeyCode::Char('w') if ctrl => {
                let start = self.word_start(self.cursor);
                self.delete(start, self.cursor);
            }
            KeyCode::Char('u') if ctrl => {
                let start = self.line_start(self.cursor);
                let start = if start == self.cursor && start > 0 {
                    start - 1
                } else {
                    start
                };
                self.delete(start, self.cursor);
            }
            KeyCode::Char('k') if ctrl => {
                let end = self.line_end(self.cursor);
                let end = if end == self.cursor && end < self.text.len() {
                    end + 1
                } else {
                    end
                };
                self.delete(self.cursor, end);
            }
            KeyCode::Char('o') if ctrl => {
                self.expand_paste();
            }
            KeyCode::Char(_) if ctrl => return Edit::Ignored,
            KeyCode::Char(c) => self.insert(c.encode_utf8(&mut [0; 4])),
            KeyCode::Backspace if alt => {
                let start = self.word_start(self.cursor);
                self.delete(start, self.cursor);
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let start = self.previous_boundary(self.cursor);
                    self.delete(start, self.cursor);
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.text.len() {
                    let end = self.next_boundary(self.cursor);
                    self.delete(self.cursor, end);
                }
            }
            KeyCode::Left if alt || ctrl => self.cursor = self.settle(self.word_start(self.cursor)),
            KeyCode::Right if alt || ctrl => self.cursor = self.settle(self.word_end(self.cursor)),
            KeyCode::Left => self.cursor = self.previous_boundary(self.cursor),
            KeyCode::Right => self.cursor = self.next_boundary(self.cursor),
            KeyCode::Home => self.cursor = self.line_start(self.cursor),
            KeyCode::End => self.cursor = self.line_end(self.cursor),
            KeyCode::Up => {
                if !self.move_row(-1) && !self.recall(-1) {
                    return Edit::Ignored;
                }
            }
            KeyCode::Down => {
                if !self.move_row(1) && !self.recall(1) {
                    return Edit::Ignored;
                }
            }
            _ => return Edit::Ignored,
        }
        Edit::Handled
    }

    /// The editor as lines `width` columns wide, the first starting with `prompt` and the rest
    /// indented as far, and where the cursor is in them. Long lines wrap at the width.
    pub fn render(
        &self,
        prompt: &str,
        width: usize,
        theme: &Theme,
    ) -> (Vec<Line<'static>>, Position) {
        let prompt_width = crate::text::width(prompt);
        let indent = " ".repeat(prompt_width);
        let width = width.max(prompt_width + 2);
        // Each row: its prefix, then its text.
        let mut rows: Vec<(Span<'static>, Vec<Span<'static>>)> =
            vec![(Span::styled(prompt.to_string(), theme.accent()), Vec::new())];
        let mut column = prompt_width;
        let mut cursor = None;
        let mut offset = 0;
        let new_row = |rows: &mut Vec<(Span<'static>, Vec<Span<'static>>)>| {
            rows.push((Span::raw(indent.clone()), Vec::new()));
        };
        loop {
            if offset == self.cursor && cursor.is_none() {
                if column >= width {
                    new_row(&mut rows);
                    column = prompt_width;
                }
                cursor = Some(Position::new(column as u16, (rows.len() - 1) as u16));
            }
            if offset >= self.text.len() {
                break;
            }
            if let Some(paste) = self.pastes.iter().find(|p| p.start == offset) {
                let label = self.text[paste.start..paste.end].to_string();
                let w = crate::text::width(&label);
                if column + w > width && column > prompt_width {
                    new_row(&mut rows);
                    column = prompt_width;
                }
                if let Some((_, content)) = rows.last_mut() {
                    content.push(Span::styled(label, theme.dim()));
                }
                column += w;
                offset = paste.end;
                continue;
            }
            let c = self.text[offset..].chars().next().unwrap_or(' ');
            offset += c.len_utf8();
            if c == '\n' {
                new_row(&mut rows);
                column = prompt_width;
                continue;
            }
            let shown = sanitize(c.encode_utf8(&mut [0; 4]));
            let w: usize = shown.chars().map(|c| c.width().unwrap_or(0)).sum();
            if column + w > width {
                new_row(&mut rows);
                column = prompt_width;
            }
            if let Some((_, content)) = rows.last_mut() {
                match content.last_mut() {
                    Some(last) if last.style == theme.plain() => {
                        last.content.to_mut().push_str(&shown)
                    }
                    _ => content.push(Span::styled(shown, theme.plain())),
                }
            }
            column += w;
        }
        let lines = rows
            .into_iter()
            .map(|(prefix, mut content)| {
                content.insert(0, prefix);
                Line::from(content)
            })
            .collect();
        (lines, cursor.unwrap_or_default())
    }
}
```

In `crates/harness-tui/src/lib.rs`:

Replace:

```rust
//! This crate draws with `ratatui`; `harness-cli` sets up the session and starts it.

pub mod diff;
pub mod highlight;
pub mod inline;
pub mod markdown;
```

with:

```rust
//! This crate draws with `ratatui`; `harness-cli` sets up the session and starts it.

pub mod diff;
pub mod editor;
pub mod highlight;
pub mod inline;
pub mod markdown;
```

- [ ] **Step 4: Run the tests, and lint**

Run: `cargo test -p harness-tui --test editor`
Expected: PASS (7 tests).

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/harness-tui
git commit -F - <<'EOF'
feat(tui): edit input with history and collapsed pastes

The input editor handles multi-line text (Alt+Enter, Shift+Enter,
Ctrl+J, or a backslash before Enter), word and line movement, and Up
and Down to recall earlier inputs. A paste over 10 lines or 1,000
characters shows as [Pasted text #n, N lines], deletes as a unit,
expands with Ctrl+O, and is sent in full. Wide characters take two
columns.

<trailer lines from the controller>
EOF
```

---

### Task 5: `/` and `@` completion

**Files:**
- Modify: `Cargo.toml`, `crates/harness-tui/Cargo.toml`, `crates/harness-tui/src/editor.rs`, `crates/harness-tui/src/lib.rs`
- Create: `crates/harness-tui/src/complete.rs`
- Test: `crates/harness-tui/tests/complete.rs`

**Interfaces:**
- Consumes: Task 4's `Editor` (and its private `splice`).
- Produces:
  - `complete::Completer`: `new(commands: Vec<(String, String)>, workspace: &Path)`, `offer(text, cursor) -> Option<Offer>`, `commands()`;
  - `complete::Offer { replace: Range<usize>, items: Vec<Item> }`, `complete::Item { insert: String, detail: String }`, `complete::MAX_ITEMS` (8);
  - `complete::render(&Offer, selected, width, &Theme) -> Vec<Line>`;
  - `Editor::replace_word(range, with)`: puts the chosen item in place of the word, with a space after it, keeping collapsed pastes elsewhere.

- [ ] **Step 1: Write the failing tests**

In `Cargo.toml`:

Replace:

```toml
libc = "0.2"
jsonschema = "0.57.0"
nix = { version = "0.31.3", features = ["signal", "process"] }
pulldown-cmark = { version = "0.13.4", default-features = false }
ratatui = { version = "0.30.2", default-features = false, features = ["crossterm"] }
regex = "1"
```

with:

```toml
libc = "0.2"
jsonschema = "0.57.0"
nix = { version = "0.31.3", features = ["signal", "process"] }
nucleo-matcher = "0.3.1"
pulldown-cmark = { version = "0.13.4", default-features = false }
ratatui = { version = "0.30.2", default-features = false, features = ["crossterm"] }
regex = "1"
```

In `crates/harness-tui/Cargo.toml`:

Replace:

```toml

[dependencies]
harness-core.workspace = true
pulldown-cmark.workspace = true
ratatui.workspace = true
serde_json.workspace = true
similar.workspace = true
syntect.workspace = true
unicode-width.workspace = true
```

with:

```toml

[dependencies]
harness-core.workspace = true
ignore.workspace = true
nucleo-matcher.workspace = true
pulldown-cmark.workspace = true
ratatui.workspace = true
serde_json.workspace = true
similar.workspace = true
syntect.workspace = true
unicode-width.workspace = true

[dev-dependencies]
tempfile.workspace = true
```

Create `crates/harness-tui/tests/complete.rs`:

```rust
//! `/` and `@` completion.

use harness_tui::{
    complete::{self, Completer, Offer},
    editor::Editor,
    style::Theme,
    text::plain,
};
use ratatui::style::Modifier;

fn commands() -> Vec<(String, String)> {
    [
        ("help", "List commands"),
        ("init", "Draft an AGENTS.md for this project"),
        ("opsx:propose", "Propose a change"),
        ("opsx:apply", "Implement a change"),
        ("plan", "Plan a change"),
    ]
    .into_iter()
    .map(|(n, d)| (n.to_string(), d.to_string()))
    .collect()
}

fn inserts(offer: &Option<Offer>) -> Vec<String> {
    offer
        .as_ref()
        .map(|o| o.items.iter().map(|i| i.insert.clone()).collect())
        .unwrap_or_default()
}

#[test]
fn a_slash_at_the_start_offers_commands_with_their_descriptions() {
    let dir = tempfile::tempdir().unwrap();
    let mut completer = Completer::new(commands(), dir.path());
    let all = completer.offer("/", 1).unwrap();
    assert_eq!(all.items.len(), 5);
    assert_eq!(all.items[0].detail, "List commands");
    assert_eq!(
        inserts(&completer.offer("/op", 3)),
        ["/opsx:propose", "/opsx:apply"]
    );
    // Names that contain what was typed come after those that start with it.
    assert_eq!(
        inserts(&completer.offer("/p", 2)),
        ["/plan", "/help", "/opsx:propose", "/opsx:apply"]
    );
    assert_eq!(completer.offer("/op", 3).unwrap().replace, 0..3);
    // Not after other text, and not once the arguments begin.
    assert!(completer.offer("see /he", 7).is_none());
    assert!(completer.offer("/help me", 8).is_none());
    assert!(completer.offer("/zzz", 4).is_none());
}

#[test]
fn an_at_offers_workspace_files_matched_fuzzily() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    for file in [
        "src/main.rs",
        "src/lib.rs",
        "docs/maintenance.md",
        "target/debug/main.rs",
        ".git/config",
        ".github/workflows/ci.yml",
    ] {
        let path = ws.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x").unwrap();
    }
    std::fs::write(ws.join(".gitignore"), "target/\n").unwrap();
    let mut completer = Completer::new(commands(), ws);
    let text = "look at @mainrs";
    let offer = completer.offer(text, text.len());
    assert_eq!(inserts(&offer)[0], "@src/main.rs");
    assert_eq!(offer.as_ref().unwrap().replace, 8..text.len());
    let everything = inserts(&completer.offer("@", 1));
    assert!(everything.contains(&"@.github/workflows/ci.yml".to_string()));
    assert!(
        !everything
            .iter()
            .any(|p| p.contains("target") || p.contains(".git/"))
    );
    // An @ inside a word, such as an email address, is not a file.
    assert!(completer.offer("mail me@example", 15).is_none());
    // The cursor must be at the end of the word.
    assert!(completer.offer("@mainrs x", 3).is_none());
}

#[test]
fn the_list_shows_names_and_descriptions_and_highlights_the_selection() {
    let dir = tempfile::tempdir().unwrap();
    let mut completer = Completer::new(commands(), dir.path());
    let offer = completer.offer("/op", 3).unwrap();
    let lines = complete::render(&offer, 1, 40, &Theme::monochrome());
    let text: Vec<String> = lines.iter().map(plain).collect();
    assert_eq!(
        text,
        [
            "  /opsx:propose  Propose a change",
            "  /opsx:apply    Implement a change",
        ]
    );
    assert!(
        lines[1].spans[0]
            .style
            .add_modifier
            .contains(Modifier::REVERSED)
    );
    assert!(
        !lines[0].spans[0]
            .style
            .add_modifier
            .contains(Modifier::REVERSED)
    );
}

#[test]
fn a_completed_word_is_replaced_and_collapsed_pastes_stay_collapsed() {
    let mut editor = Editor::new(Vec::new());
    let long: String = (1..=11).map(|i| format!("{i}\n")).collect();
    editor.paste(&long);
    editor.insert(" see @mainrs  please");
    let start = editor.text().find('@').unwrap();
    editor.replace_word(start..start + "@mainrs".len(), "@src/main.rs");
    assert_eq!(
        editor.text(),
        "[Pasted text #1, 11 lines] see @src/main.rs please"
    );
    assert_eq!(
        &editor.text()[..editor.cursor()],
        "[Pasted text #1, 11 lines] see @src/main.rs "
    );
    assert_eq!(editor.expanded(), format!("{long} see @src/main.rs please"));
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p harness-tui --test complete`
Expected: FAIL to compile: ``unresolved import `harness_tui::complete` `` and ``no method named `replace_word` found for struct `Editor` ``.

- [ ] **Step 3: Completion**

Create `crates/harness-tui/src/complete.rs`:

```rust
//! Completion while typing: `/` at the start of the input offers commands with their
//! descriptions, and `@` offers workspace files, matched fuzzily (`@mainrs` finds
//! `src/main.rs`).

use std::{
    ops::Range,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use nucleo_matcher::{
    Config, Matcher,
    pattern::{CaseMatching, Normalization, Pattern},
};
use ratatui::text::{Line, Span};

use crate::{
    style::Theme,
    text::{sanitize, width as text_width},
};

/// Items offered at most.
pub const MAX_ITEMS: usize = 8;
/// Workspace files indexed at most, and how long indexing may take.
const MAX_FILES: usize = 20_000;
const INDEX_TIME: Duration = Duration::from_secs(1);

/// One thing completion can insert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// What replaces the word being completed.
    pub insert: String,
    /// Shown next to it: a command's description.
    pub detail: String,
}

/// What completion offers for the word at the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    /// The bytes of the input the chosen item replaces.
    pub replace: Range<usize>,
    pub items: Vec<Item>,
}

/// Completes commands and workspace files.
pub struct Completer {
    /// Command names (without `/`) and descriptions.
    commands: Vec<(String, String)>,
    workspace: PathBuf,
    /// The workspace's files, relative to it, read the first time `@` is typed.
    files: Option<Vec<String>>,
    matcher: Matcher,
}

impl Completer {
    pub fn new(commands: Vec<(String, String)>, workspace: &Path) -> Completer {
        Completer {
            commands,
            workspace: workspace.to_path_buf(),
            files: None,
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
        }
    }

    /// The commands and their descriptions, as given.
    pub fn commands(&self) -> &[(String, String)] {
        &self.commands
    }

    /// What to offer for `text` with the cursor at byte `cursor`, if anything.
    pub fn offer(&mut self, text: &str, cursor: usize) -> Option<Offer> {
        let before = &text[..cursor];
        // The word the cursor is at the end of.
        let start = before.rfind(char::is_whitespace).map_or(0, |i| {
            i + before[i..].chars().next().map_or(1, char::len_utf8)
        });
        let word = &before[start..];
        let at_word_end = text[cursor..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace);
        if !at_word_end {
            return None;
        }
        if let Some(name) = word.strip_prefix('/')
            && text[..start].trim().is_empty()
        {
            let items = self.command_items(name);
            return (!items.is_empty()).then_some(Offer {
                replace: start..cursor,
                items,
            });
        }
        if let Some(query) = word.strip_prefix('@') {
            let items = self.file_items(query);
            return (!items.is_empty()).then_some(Offer {
                replace: start..cursor,
                items,
            });
        }
        None
    }

    fn command_items(&self, typed: &str) -> Vec<Item> {
        let item = |(name, description): &(String, String)| Item {
            insert: format!("/{name}"),
            detail: description.clone(),
        };
        let prefix = self
            .commands
            .iter()
            .filter(|(name, _)| name.starts_with(typed));
        let inside = self
            .commands
            .iter()
            .filter(|(name, _)| !name.starts_with(typed) && name.contains(typed));
        prefix.chain(inside).map(item).take(MAX_ITEMS).collect()
    }

    fn file_items(&mut self, query: &str) -> Vec<Item> {
        let workspace = self.workspace.clone();
        let files = self.files.get_or_insert_with(|| index(&workspace));
        let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
        let mut matched = pattern.match_list(files.iter(), &mut self.matcher);
        matched.sort_by(|(a, x), (b, y)| y.cmp(x).then(a.len().cmp(&b.len())).then(a.cmp(b)));
        matched
            .into_iter()
            .take(MAX_ITEMS)
            .map(|(path, _)| Item {
                insert: format!("@{path}"),
                detail: String::new(),
            })
            .collect()
    }
}

/// The workspace's files that git does not ignore, relative to it, `/`-separated, up to
/// [`MAX_FILES`] and as many as can be found in [`INDEX_TIME`].
fn index(workspace: &Path) -> Vec<String> {
    let started = Instant::now();
    let mut files = Vec::new();
    let walk = ignore::WalkBuilder::new(workspace)
        .hidden(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();
    for entry in walk.flatten() {
        if files.len() >= MAX_FILES || started.elapsed() > INDEX_TIME {
            break;
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if let Ok(relative) = entry.path().strip_prefix(workspace)
            && let Some(path) = relative.to_str()
        {
            files.push(path.to_string());
        }
    }
    files.sort();
    files
}

/// The offer as lines of a list at most `width` columns wide, the `selected` item highlighted.
pub fn render(offer: &Offer, selected: usize, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let name_width = offer
        .items
        .iter()
        .map(|i| text_width(&i.insert))
        .max()
        .unwrap_or(0)
        .min(width / 2);
    offer
        .items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let style = if i == selected {
                theme.selected()
            } else {
                theme.plain()
            };
            let name = sanitize(&item.insert);
            let pad = name_width.saturating_sub(text_width(&name));
            let mut spans = vec![Span::styled(format!("  {name}{}", " ".repeat(pad)), style)];
            if !item.detail.is_empty() {
                let room = width.saturating_sub(name_width + 4);
                let detail: String = sanitize(&item.detail)
                    .replace('\n', " ")
                    .chars()
                    .take(room)
                    .collect();
                spans.push(Span::styled(format!("  {detail}"), theme.dim()));
            }
            Line::from(spans)
        })
        .collect()
}
```

In `crates/harness-tui/src/editor.rs`:

Replace:

```rust
        self.cursor = self.text.len();
        self.pastes.clear();
        self.recall = None;
    }

    /// Empties the editor.
```

with:

```rust
        self.cursor = self.text.len();
        self.pastes.clear();
        self.recall = None;
    }

    /// Replaces the bytes `range` of the text (a word being completed) with `with` and a space,
    /// dropping spaces that followed the word, and puts the cursor after the space. Collapsed
    /// pastes elsewhere in the text stay collapsed.
    pub fn replace_word(&mut self, range: std::ops::Range<usize>, with: &str) {
        let end = range.end.min(self.text.len());
        let spaces = self.text[end..].len() - self.text[end..].trim_start_matches(' ').len();
        let start = range.start.min(end);
        self.splice(start, end + spaces, &format!("{with} "));
        self.cursor = start + with.len() + 1;
    }

    /// Empties the editor.
```

In `crates/harness-tui/src/lib.rs`:

Replace:

```rust
//! only a small live region at the bottom (the input, what is streaming, prompts) is redrawn.
//! This crate draws with `ratatui`; `harness-cli` sets up the session and starts it.

pub mod diff;
pub mod editor;
pub mod highlight;
```

with:

```rust
//! only a small live region at the bottom (the input, what is streaming, prompts) is redrawn.
//! This crate draws with `ratatui`; `harness-cli` sets up the session and starts it.

pub mod complete;
pub mod diff;
pub mod editor;
pub mod highlight;
```

- [ ] **Step 4: Run the tests, lint, and check the dependencies**

Run: `cargo test -p harness-tui`
Expected: PASS, including `an_at_offers_workspace_files_matched_fuzzily`.

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo deny check`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/harness-tui
git commit -F - <<'EOF'
feat(tui): complete commands after / and files after @

A slash at the start of the input offers the commands whose names
start with, then contain, what was typed, with their descriptions. An
@ offers the workspace's files that git does not ignore, matched
fuzzily with nucleo-matcher, so @mainrs finds src/main.rs.

<trailer lines from the controller>
EOF
```

---

### Task 6: The interactive session: `harness` on its own

**Files:**
- Modify: `Cargo.toml`, `crates/harness-tui/Cargo.toml`, `crates/harness-tui/src/lib.rs`, `crates/harness-cli/Cargo.toml`, `crates/harness-cli/src/{ask,main,prompt,slash}.rs`, `crates/harness-cli/tests/cli_smoke.rs`
- Create: `crates/harness-tui/src/{app,ui,terminal}.rs`, `crates/harness-cli/src/{start,interactive}.rs`
- Test: `crates/harness-tui/tests/session.rs`; the test modules of `prompt.rs`, `start.rs` and `interactive.rs`

**Interfaces:**
- Consumes: Tasks 2 to 5 (`Theme`, `InlineTerminal`, `Transcript`, `Editor`, `Completer`); P3's `Agent::run_turn`, `Agent::rewind_points`, `harness_context::commands::{parse_invocation, is_builtin, Commands::listing}`.
- Produces:
  - `app::Host` (what the CLI provides): `is_command(&self, name) -> bool`, `prepare(&mut self, typed) -> Prepared { input: TurnInput, notes, warnings }`;
  - `app::Options { theme, model, mode, commands, workspace, history }` (later tasks add fields), `app::App` (`new(Options, Box<dyn Host>, width)`, `on_key(KeyEvent, Instant) -> Option<Action>`, `on_paste`, `on_event`, `live(rows) -> (Vec<Line>, Position)`, `editor()`, `busy()`, `mode()`, `set_width`, `pub transcript`), `app::Action { Run(TurnInput), Interrupt, Quit }`, `app::QUIT_WINDOW` (2 s);
  - `ui::Ui<B>`: `start(agent, host, term, options)` (the agent moves to a task of its own), `handle(Event) -> io::Result<Flow>`, `next()`, `settle()` (tests), `draw()`, `run(stream)`, `finish()`, `app()`, `app_mut()`, `terminal()`, `terminal_mut()`; `ui::Flow { Continue, Quit }`;
  - `terminal::RawMode` (`enable`, `disable`), `terminal::CrosstermRawMode`, `terminal::Modes<W, R>` (`enter(out, raw, keyboard)`, `suspend`, `resume`, `out()`; undone on drop);
  - in `harness-cli`: `start::start(Request { setup, mode, model, session, approver, interactive }) -> Started { agent, sandbox_session, policy }`, `start::context_window(id) -> u64` (32,768 until P4), `start::{SessionEnd, end_run, tool_context}`; `interactive::run(model, mode, choice) -> u8` and `interactive::needs_terminal(stdin, stdout)`; `slash::turn_input(…) -> Expanded { input, messages: Vec<Message> }` with `slash::print_messages`; `prompt::base_prompt(mode, sandboxed, interactive)`.

- [ ] **Step 1: Write the failing tests**

In `Cargo.toml`:

Replace:

```toml
async-stream = "0.3"
async-trait = "0.1.92"
clap = { version = "4.6.7", features = ["derive"] }
eventsource-stream = "0.2.3"
futures = "0.3.34"
globset = "0.4.20"
```

with:

```toml
async-stream = "0.3"
async-trait = "0.1.92"
clap = { version = "4.6.7", features = ["derive"] }
crossterm = { version = "0.29.0", features = ["event-stream"] }
eventsource-stream = "0.2.3"
futures = "0.3.34"
globset = "0.4.20"
```

In `crates/harness-tui/Cargo.toml`:

Replace (1 of 2):

```toml
publish.workspace = true

[dependencies]
harness-core.workspace = true
ignore.workspace = true
nucleo-matcher.workspace = true
```

with:

```toml
publish.workspace = true

[dependencies]
futures.workspace = true
harness-context.workspace = true
harness-core.workspace = true
ignore.workspace = true
nucleo-matcher.workspace = true
```

Replace (2 of 2):

```toml
serde_json.workspace = true
similar.workspace = true
syntect.workspace = true
unicode-width.workspace = true

[dev-dependencies]
```

with:

```toml
serde_json.workspace = true
similar.workspace = true
syntect.workspace = true
tokio.workspace = true
tokio-util.workspace = true
unicode-width.workspace = true

[dev-dependencies]
```

Create `crates/harness-tui/tests/session.rs`:

```rust
//! The interactive session end to end: scripted keys and pastes go in, the agent runs against
//! the mock provider, and what reaches the screen and the scrollback is checked on ratatui's
//! `TestBackend`.

use std::{
    cell::RefCell,
    path::Path,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    message::Message,
    permission::Mode,
    testing::{MockProvider, Script},
    tool::{ToolContext, ToolRegistry},
    turn::{InputPart, TurnInput},
};
use harness_tui::{
    app::{App, Host, Options, Prepared},
    inline::InlineTerminal,
    style::Theme,
    terminal::{Modes, RawMode},
    ui::{Flow, Ui},
};
use ratatui::{
    backend::TestBackend,
    buffer::Buffer,
    crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers},
};

/// Expands `/greet <name>` into a greeting, as harness-cli expands custom commands.
struct TestHost;

impl Host for TestHost {
    fn is_command(&self, name: &str) -> bool {
        name == "greet"
    }

    fn prepare(&mut self, typed: &str) -> Prepared {
        let name = typed.trim_start_matches("/greet").trim();
        Prepared {
            input: TurnInput {
                parts: vec![InputPart::Text(format!("Say hello to {name}"))],
                display: Some(typed.to_string()),
                ..TurnInput::default()
            },
            notes: vec!["/greet runs as its command file asks".into()],
            warnings: Vec::new(),
        }
    }
}

fn agent(provider: Arc<MockProvider>, dir: &Path) -> Agent {
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode: Mode::Auto,
        workspace: dir.to_path_buf(),
        read_dirs: Vec::new(),
        rules: RuleSet::default(),
        sandbox_available: false,
        writes_need_approval: false,
    }));
    Agent::new(
        provider,
        ToolRegistry::new(Vec::new()),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m", "m", "system", dir.join("out")),
        ToolContext::new(dir),
    )
}

fn options(dir: &Path) -> Options {
    Options {
        theme: Theme::monochrome(),
        model: "mock/m".into(),
        mode: Mode::Auto,
        commands: vec![
            ("help".into(), "List commands".into()),
            ("rewind".into(), "Rewind code, conversation, or both".into()),
            ("greet".into(), "Say hello".into()),
        ],
        workspace: dir.to_path_buf(),
        history: Vec::new(),
    }
}

fn start(provider: Arc<MockProvider>, dir: &Path) -> Ui<TestBackend> {
    let term = InlineTerminal::new(TestBackend::new(60, 16), 0).unwrap();
    let mut ui = Ui::start(agent(provider, dir), Box::new(TestHost), term, options(dir));
    ui.draw().unwrap();
    ui
}

fn rows(buffer: &Buffer) -> Vec<String> {
    let width = buffer.area.width as usize;
    buffer
        .content
        .chunks(width.max(1))
        .map(|row| {
            let text: String = row.iter().map(|cell| cell.symbol()).collect();
            text.trim_end().to_string()
        })
        .collect()
}

/// The scrollback, then the screen.
fn everything(ui: &Ui<TestBackend>) -> Vec<String> {
    let backend = ui.terminal().backend();
    let mut out = rows(backend.scrollback());
    out.extend(rows(backend.buffer()));
    out
}

fn shows(ui: &Ui<TestBackend>, text: &str) -> bool {
    everything(ui).iter().any(|row| row.contains(text))
}

fn press(ui: &mut Ui<TestBackend>, code: KeyCode) -> Flow {
    ui.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        .unwrap()
}

fn ctrl(ui: &mut Ui<TestBackend>, c: char) -> Flow {
    ui.handle(Event::Key(KeyEvent::new(
        KeyCode::Char(c),
        KeyModifiers::CONTROL,
    )))
    .unwrap()
}

fn type_text(ui: &mut Ui<TestBackend>, text: &str) {
    for c in text.chars() {
        press(ui, KeyCode::Char(c));
    }
}

async fn settle(ui: &mut Ui<TestBackend>) {
    tokio::time::timeout(Duration::from_secs(10), ui.settle())
        .await
        .expect("the turn ends")
        .unwrap();
}

fn last_user_message(provider: &MockProvider) -> String {
    let requests = provider.requests();
    requests
        .last()
        .and_then(|r| {
            r.messages.iter().rev().find_map(|m| match m {
                Message::User { content } => Some(content.clone()),
                _ => None,
            })
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn a_prompt_runs_a_turn_and_the_reply_goes_into_the_scrollback() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::text("Hello **there**")]);
    let mut ui = start(provider.clone(), dir.path());
    assert!(shows(&ui, "mock/m · auto"));
    type_text(&mut ui, "hi");
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    assert_eq!(last_user_message(&provider), "hi");
    let screen = everything(&ui);
    let user = screen.iter().position(|r| r == "› hi").expect("the prompt");
    let reply = screen
        .iter()
        .position(|r| r == "Hello there")
        .expect("the reply");
    assert!(user < reply, "{screen:#?}");
    // The input is empty again, under the reply.
    assert!(screen[reply..].iter().any(|r| r == "›"), "{screen:#?}");
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn esc_interrupts_a_running_turn_and_the_session_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::Hang(vec![harness_core::provider::ProviderEvent::TextDelta(
            "partial answer".into(),
        )]),
        Script::text("second answer"),
    ]);
    let mut ui = start(provider.clone(), dir.path());
    type_text(&mut ui, "go");
    press(&mut ui, KeyCode::Enter);
    while !shows(&ui, "partial answer") {
        ui.next().await.unwrap();
    }
    // Enter while a turn runs keeps the input and says how to stop the turn.
    type_text(&mut ui, "later");
    press(&mut ui, KeyCode::Enter);
    assert_eq!(ui.app().editor().text(), "later");
    assert!(shows(&ui, "a turn is running: press Esc to interrupt it"));
    press(&mut ui, KeyCode::Esc);
    settle(&mut ui).await;
    assert!(shows(&ui, "interrupted"));
    assert!(!ui.app().busy());
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    assert!(shows(&ui, "second answer"));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn ctrl_c_clears_the_input_and_exits_when_pressed_twice() {
    let dir = tempfile::tempdir().unwrap();
    let mut ui = start(MockProvider::new(Vec::new()), dir.path());
    type_text(&mut ui, "abc");
    assert_eq!(ctrl(&mut ui, 'c'), Flow::Continue);
    assert_eq!(ui.app().editor().text(), "");
    assert!(shows(&ui, "press Ctrl+C again to exit"));
    assert_eq!(ctrl(&mut ui, 'c'), Flow::Quit);
    ui.finish().await.unwrap();
    // Ctrl+D on empty input exits too.
    let mut ui = start(MockProvider::new(Vec::new()), dir.path());
    assert_eq!(ctrl(&mut ui, 'd'), Flow::Quit);
    ui.finish().await.unwrap();
}

#[test]
fn a_second_ctrl_c_after_two_seconds_does_not_exit() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = App::new(options(dir.path()), Box::new(TestHost), 60);
    let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let start = Instant::now();
    assert!(app.on_key(key, start).is_none());
    assert!(app.on_key(key, start + Duration::from_secs(3)).is_none());
    assert!(matches!(
        app.on_key(key, start + Duration::from_millis(4500)),
        Some(harness_tui::app::Action::Quit)
    ));
}

#[tokio::test]
async fn help_lists_the_commands_and_the_keys() {
    let dir = tempfile::tempdir().unwrap();
    let mut ui = start(MockProvider::new(Vec::new()), dir.path());
    type_text(&mut ui, "/help");
    press(&mut ui, KeyCode::Enter);
    assert!(shows(&ui, "/greet  Say hello"));
    assert!(shows(&ui, "Esc  interrupt the running turn"));
    assert!(!ui.app().busy());
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn later_built_ins_say_so_and_unknown_commands_are_explained() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(Vec::new());
    let mut ui = start(provider.clone(), dir.path());
    type_text(&mut ui, "/rewind");
    press(&mut ui, KeyCode::Esc);
    press(&mut ui, KeyCode::Enter);
    assert!(shows(&ui, "/rewind is not available yet"));
    type_text(&mut ui, "/nope");
    press(&mut ui, KeyCode::Enter);
    assert!(shows(&ui, "unknown command /nope"));
    assert_eq!(ui.app().editor().text(), "/nope");
    assert!(provider.requests().is_empty());
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn a_custom_command_runs_as_the_host_expands_it() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::text("Hello, Ann!")]);
    let mut ui = start(provider.clone(), dir.path());
    type_text(&mut ui, "/gr");
    // Tab completes the command.
    press(&mut ui, KeyCode::Tab);
    assert_eq!(ui.app().editor().text(), "/greet ");
    type_text(&mut ui, "Ann");
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    assert_eq!(last_user_message(&provider), "Say hello to Ann");
    assert!(shows(&ui, "› /greet Ann"));
    assert!(shows(&ui, "/greet runs as its command file asks"));
    assert!(shows(&ui, "Hello, Ann!"));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn a_long_paste_is_shown_collapsed_and_sent_in_full() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::text("Looks like a null pointer.")]);
    let mut ui = start(provider.clone(), dir.path());
    let trace: String = (1..=200).map(|i| format!("at frame {i}\n")).collect();
    type_text(&mut ui, "why? ");
    ui.handle(Event::Paste(trace.clone())).unwrap();
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    assert!(shows(&ui, "› why? [Pasted text #1, 200 lines]"));
    assert!(!shows(&ui, "at frame 200"));
    assert_eq!(last_user_message(&provider), format!("why? {trace}"));
    ui.finish().await.unwrap();
}

/// Records whether raw mode is on.
struct FakeRaw(Rc<RefCell<Vec<&'static str>>>);

impl RawMode for FakeRaw {
    fn enable(&mut self) -> std::io::Result<()> {
        self.0.borrow_mut().push("raw on");
        Ok(())
    }
    fn disable(&mut self) -> std::io::Result<()> {
        self.0.borrow_mut().push("raw off");
        Ok(())
    }
}

#[test]
fn the_terminal_modes_are_set_and_undone_in_reverse_order() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut out = Vec::new();
    {
        let mut modes = Modes::enter(&mut out, FakeRaw(log.clone()), true).unwrap();
        assert_eq!(*log.borrow(), ["raw on"]);
        assert_eq!(modes.out().as_slice(), b"\x1b[?2004h\x1b[>1u");
        modes.suspend().unwrap();
        modes.resume().unwrap();
    }
    assert_eq!(*log.borrow(), ["raw on", "raw off", "raw on", "raw off"]);
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "\x1b[?2004h\x1b[>1u\x1b[<1u\x1b[?2004l\x1b[?2004h\x1b[>1u\x1b[<1u\x1b[?2004l"
    );
    // Without disambiguated keys, only bracketed paste.
    let mut out = Vec::new();
    drop(Modes::enter(&mut out, FakeRaw(log.clone()), false).unwrap());
    assert_eq!(String::from_utf8(out).unwrap(), "\x1b[?2004h\x1b[?2004l");
}

// Review Focus: leaving while a turn runs must stop it and let the session end, not hang on the
// turn or leave the agent (and its session file) behind.
#[tokio::test]
async fn quitting_while_a_turn_runs_stops_it_and_ends_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::Hang(vec![
        harness_core::provider::ProviderEvent::TextDelta("still going".into()),
    ])]);
    let mut ui = start(provider, dir.path());
    type_text(&mut ui, "go");
    press(&mut ui, KeyCode::Enter);
    while !shows(&ui, "still going") {
        ui.next().await.unwrap();
    }
    assert_eq!(ctrl(&mut ui, 'c'), Flow::Continue);
    assert_eq!(ctrl(&mut ui, 'c'), Flow::Quit);
    tokio::time::timeout(Duration::from_secs(10), ui.finish())
        .await
        .expect("the session ends")
        .unwrap();
    // What streamed is kept, and the live region is gone.
    assert!(shows(&ui, "still going"));
    assert!(!shows(&ui, "mock/m · auto"));
}
```

The interactive base prompt's tests. In `crates/harness-cli/src/prompt.rs`:

Replace (1 of 6):

```rust
            Mode::FullAccess,
        ] {
            for sandboxed in [true, false] {
                let prompt = base_prompt(mode, sandboxed);
                assert!(prompt.len() / 4 < 1000, "~{} tokens", prompt.len() / 4);
            }
        }
    }
```

with:

```rust
            Mode::FullAccess,
        ] {
            for sandboxed in [true, false] {
                for interactive in [true, false] {
                    let prompt = base_prompt(mode, sandboxed, interactive);
                    assert!(prompt.len() / 4 < 1000, "~{} tokens", prompt.len() / 4);
                }
            }
        }
    }
```

Replace (2 of 6):

```rust
    // know its mode won't let it ask.
    #[test]
    fn approval_mode_line_names_the_mode() {
        let prompt = base_prompt(Mode::Ask, true);
        assert!(prompt.contains("Approval mode: ask"), "{prompt}");
    }

    #[test]
    fn the_prompt_says_whether_commands_are_sandboxed() {
        let yes = base_prompt(Mode::Auto, true);
        assert!(yes.contains("run in a sandbox"), "{yes}");
        let no = base_prompt(Mode::Auto, false);
        assert!(no.contains("No OS sandbox is active"), "{no}");
        assert!(!no.contains("run in a sandbox"), "{no}");
    }
```

with:

```rust
    // know its mode won't let it ask.
    #[test]
    fn approval_mode_line_names_the_mode() {
        let prompt = base_prompt(Mode::Ask, true, false);
        assert!(prompt.contains("Approval mode: ask"), "{prompt}");
    }

    #[test]
    fn the_prompt_says_whether_commands_are_sandboxed() {
        let yes = base_prompt(Mode::Auto, true, false);
        assert!(yes.contains("run in a sandbox"), "{yes}");
        let no = base_prompt(Mode::Auto, false, false);
        assert!(no.contains("No OS sandbox is active"), "{no}");
        assert!(!no.contains("run in a sandbox"), "{no}");
    }
```

Replace (3 of 6):

```rust
    // implying ordinary commands still run.
    #[test]
    fn without_a_sandbox_the_prompt_says_every_command_needs_approval_unless_full_access() {
        let auto = base_prompt(Mode::Auto, false);
        assert!(
            auto.contains("every shell command needs approval"),
            "{auto}"
        );
        let full_access = base_prompt(Mode::FullAccess, false);
        assert!(!full_access.contains("every shell command needs approval"));
    }

    #[test]
    fn plan_and_read_only_without_a_sandbox_say_commands_are_refused() {
        for mode in [Mode::Plan, Mode::ReadOnly] {
            let prompt = base_prompt(mode, false);
            assert!(
                prompt.contains("shell commands and file edits are refused"),
                "{prompt}"
```

with:

```rust
    // implying ordinary commands still run.
    #[test]
    fn without_a_sandbox_the_prompt_says_every_command_needs_approval_unless_full_access() {
        let auto = base_prompt(Mode::Auto, false, false);
        assert!(
            auto.contains("every shell command needs approval"),
            "{auto}"
        );
        let full_access = base_prompt(Mode::FullAccess, false, false);
        assert!(!full_access.contains("every shell command needs approval"));
    }

    #[test]
    fn plan_and_read_only_without_a_sandbox_say_commands_are_refused() {
        for mode in [Mode::Plan, Mode::ReadOnly] {
            let prompt = base_prompt(mode, false, false);
            assert!(
                prompt.contains("shell commands and file edits are refused"),
                "{prompt}"
```

Replace (4 of 6):

```rust
    #[test]
    fn plan_and_read_only_get_a_read_only_sandbox_line() {
        for mode in [Mode::Plan, Mode::ReadOnly] {
            let prompt = base_prompt(mode, true);
            assert!(prompt.contains("read-only sandbox"), "{prompt}");
        }
    }

    #[test]
    fn plan_mode_sandboxed_says_file_edits_are_refused() {
        let prompt = base_prompt(Mode::Plan, true);
        assert!(prompt.contains("file edits are refused"), "{prompt}");
    }

```

with:

```rust
    #[test]
    fn plan_and_read_only_get_a_read_only_sandbox_line() {
        for mode in [Mode::Plan, Mode::ReadOnly] {
            let prompt = base_prompt(mode, true, false);
            assert!(prompt.contains("read-only sandbox"), "{prompt}");
        }
    }

    #[test]
    fn plan_mode_sandboxed_says_file_edits_are_refused() {
        let prompt = base_prompt(Mode::Plan, true, false);
        assert!(prompt.contains("file edits are refused"), "{prompt}");
    }

```

Replace (5 of 6):

```rust
    fn ask_and_auto_state_the_refusal_rule_sandboxed_or_not() {
        for mode in [Mode::Ask, Mode::Auto] {
            for sandboxed in [true, false] {
                let prompt = base_prompt(mode, sandboxed);
                assert!(prompt.contains("non-interactively"), "{prompt}");
                assert!(
                    prompt.contains("actions that need approval will be refused"),
```

with:

```rust
    fn ask_and_auto_state_the_refusal_rule_sandboxed_or_not() {
        for mode in [Mode::Ask, Mode::Auto] {
            for sandboxed in [true, false] {
                let prompt = base_prompt(mode, sandboxed, false);
                assert!(prompt.contains("non-interactively"), "{prompt}");
                assert!(
                    prompt.contains("actions that need approval will be refused"),
```

Replace (6 of 6):

```rust

    #[test]
    fn full_access_states_the_full_access_rule_instead_of_the_refusal_rule() {
        let prompt = base_prompt(Mode::FullAccess, false);
        assert!(
            prompt.contains("full-access mode actions run without approval"),
            "{prompt}"
        );
        assert!(!prompt.contains("actions that need approval will be refused"));
    }
}
```

with:

```rust

    #[test]
    fn full_access_states_the_full_access_rule_instead_of_the_refusal_rule() {
        let prompt = base_prompt(Mode::FullAccess, false, false);
        assert!(
            prompt.contains("full-access mode actions run without approval"),
            "{prompt}"
        );
        assert!(!prompt.contains("actions that need approval will be refused"));
    }

    #[test]
    fn the_interactive_prompt_says_the_user_answers_approvals() {
        for mode in [Mode::Ask, Mode::Auto] {
            let prompt = base_prompt(mode, true, true);
            assert!(prompt.contains("The user is at the terminal"), "{prompt}");
            assert!(!prompt.contains("non-interactively"), "{prompt}");
            let unsandboxed = base_prompt(mode, false, true);
            assert!(
                unsandboxed.contains("every shell command needs the user's approval"),
                "{unsandboxed}"
            );
            assert!(!unsandboxed.contains("will be refused"), "{unsandboxed}");
        }
        // Full-access asks nobody, interactive or not.
        assert_eq!(
            base_prompt(Mode::FullAccess, false, true),
            base_prompt(Mode::FullAccess, false, false)
        );
    }
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p harness-tui --test session`
Expected: FAIL to compile: ``unresolved imports `harness_tui::app`, `harness_tui::terminal`, `harness_tui::ui` ``.

Run: `cargo test -p harness-cli --bin harness prompt`
Expected: FAIL to compile: ``this function takes 2 arguments but 3 arguments were supplied`` (`base_prompt`).

- [ ] **Step 3: The terminal's modes**

Create `crates/harness-tui/src/terminal.rs`:

```rust
//! The terminal's modes while harness runs: raw mode (keys arrive one by one, Ctrl+C and Ctrl+S
//! included, since raw mode also turns off XON/XOFF flow control), bracketed paste (a paste
//! arrives as one event), and, where the terminal supports it, disambiguated keys (so
//! Shift+Enter differs from Enter). They are undone in reverse order when harness leaves, or
//! hands the terminal to an editor.

use std::io::{self, Write};

use ratatui::crossterm::{
    event::{
        DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    queue, terminal,
};

/// Turns the terminal's raw mode on and off.
pub trait RawMode {
    fn enable(&mut self) -> io::Result<()>;
    fn disable(&mut self) -> io::Result<()>;
}

/// Raw mode on the process's terminal, through crossterm (`cfmakeraw`, which also clears
/// `IXON`, so Ctrl+S and Ctrl+Q reach harness as keys).
pub struct CrosstermRawMode;

impl RawMode for CrosstermRawMode {
    fn enable(&mut self) -> io::Result<()> {
        terminal::enable_raw_mode()
    }

    fn disable(&mut self) -> io::Result<()> {
        terminal::disable_raw_mode()
    }
}

/// The modes harness sets, undone when dropped.
pub struct Modes<W: Write, R: RawMode> {
    out: W,
    raw: R,
    /// The terminal reports disambiguated keys (the kitty keyboard protocol).
    keyboard: bool,
    active: bool,
}

impl<W: Write, R: RawMode> Modes<W, R> {
    /// Sets the modes, writing their escape sequences to `out`. `keyboard` asks for
    /// disambiguated keys, for terminals that support them.
    pub fn enter(out: W, raw: R, keyboard: bool) -> io::Result<Self> {
        let mut modes = Modes {
            out,
            raw,
            keyboard,
            active: false,
        };
        modes.resume()?;
        Ok(modes)
    }

    /// Sets the modes again after [`suspend`](Self::suspend).
    pub fn resume(&mut self) -> io::Result<()> {
        if self.active {
            return Ok(());
        }
        self.raw.enable()?;
        self.active = true;
        queue!(self.out, EnableBracketedPaste)?;
        if self.keyboard {
            queue!(
                self.out,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )?;
        }
        self.out.flush()
    }

    /// Undoes the modes, in reverse order, so another program (an editor) or the shell gets the
    /// terminal as it was.
    pub fn suspend(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        if self.keyboard {
            queue!(self.out, PopKeyboardEnhancementFlags)?;
        }
        queue!(self.out, DisableBracketedPaste)?;
        self.out.flush()?;
        self.raw.disable()
    }

    pub fn out(&self) -> &W {
        &self.out
    }
}

impl<W: Write, R: RawMode> Drop for Modes<W, R> {
    fn drop(&mut self) {
        let _ = self.suspend();
    }
}
```

- [ ] **Step 4: The app**

Create `crates/harness-tui/src/app.rs`:

```rust
//! What the interactive session shows and how it reacts to keys, apart from the terminal and
//! the running agent: the transcript, the input editor with completion, and the status line.
//! Keys and events go in; lines for the scrollback, the live region, and actions for the
//! session to carry out come out.

use std::time::{Duration, Instant};

use harness_context::commands::{is_builtin, parse_invocation};
use harness_core::{event::AgentEvent, permission::Mode, turn::TurnInput};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::Position,
    text::{Line, Span},
};

use crate::{
    complete::{self, Completer, Offer},
    editor::{Edit, Editor},
    style::Theme,
    text::{sanitize, wrap},
    transcript::Transcript,
};

/// Ctrl+C twice within this long exits.
pub const QUIT_WINDOW: Duration = Duration::from_secs(2);

/// Built-in commands that come with the rest of the terminal UI, and where.
const LATER: [(&str, &str); 7] = [
    ("model", "with model profiles (P4) and the model picker"),
    ("login", "with provider sign-in (P4)"),
    (
        "mode",
        "with the full terminal UI; press Shift+Tab to cycle plan, ask and auto",
    ),
    ("new", "with the session picker"),
    (
        "resume",
        "with the session picker; start harness with -c or --resume <id>",
    ),
    ("rewind", "with the rewind picker"),
    ("compact", "with the full terminal UI"),
];

/// A slash command expanded for a turn.
pub struct Prepared {
    pub input: TurnInput,
    /// Shown dim before the turn.
    pub notes: Vec<String>,
    pub warnings: Vec<String>,
}

/// What the session provides that the UI cannot know by itself: the custom commands, and how
/// they and `/init` expand.
pub trait Host: Send {
    /// Whether `/name` is a custom command.
    fn is_command(&self, name: &str) -> bool;
    /// The turn for `typed`, which names a custom command or `/init`.
    fn prepare(&mut self, typed: &str) -> Prepared;
}

/// Settings of the interactive session.
pub struct Options {
    pub theme: Theme,
    /// The session's model, `<provider>/<model>`.
    pub model: String,
    pub mode: Mode,
    /// Commands and their descriptions, built-ins first, for `/help` and completion.
    pub commands: Vec<(String, String)>,
    pub workspace: std::path::PathBuf,
    /// Earlier inputs, oldest first, for Up.
    pub history: Vec<String>,
}

/// What the session should do after a key.
#[derive(Debug)]
pub enum Action {
    /// Start a turn.
    Run(TurnInput),
    /// Stop the running turn.
    Interrupt,
    /// Leave harness.
    Quit,
}

/// The completion list being shown.
struct Completion {
    offer: Offer,
    selected: usize,
}

pub struct App {
    pub transcript: Transcript,
    editor: Editor,
    completer: Completer,
    completion: Option<Completion>,
    host: Box<dyn Host>,
    model: String,
    mode: Mode,
    /// A line under the status line, such as how to exit.
    hint: Option<String>,
    /// When Ctrl+C was last pressed.
    ctrl_c: Option<Instant>,
    /// From the moment a turn is asked for until it has finished.
    running: bool,
    width: usize,
}

impl App {
    pub fn new(options: Options, host: Box<dyn Host>, width: usize) -> App {
        App {
            transcript: Transcript::new(options.theme),
            editor: Editor::new(options.history),
            completer: Completer::new(options.commands, &options.workspace),
            completion: None,
            host,
            model: options.model,
            mode: options.mode,
            hint: None,
            ctrl_c: None,
            running: false,
            width,
        }
    }

    fn theme(&self) -> Theme {
        *self.transcript.theme()
    }

    /// The screen's width changed.
    pub fn set_width(&mut self, width: usize) {
        self.width = width;
    }

    pub fn editor(&self) -> &Editor {
        &self.editor
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Whether a turn is running, or about to.
    pub fn busy(&self) -> bool {
        self.running
    }

    /// Takes in an event from the agent.
    pub fn on_event(&mut self, event: &AgentEvent) {
        if let AgentEvent::TurnFinished { .. } = event {
            self.running = false;
        }
        self.transcript.on_event(event, self.width);
    }

    /// Asks for a turn.
    fn run(&mut self, input: TurnInput) -> Option<Action> {
        self.running = true;
        Some(Action::Run(input))
    }

    /// Takes in a paste.
    pub fn on_paste(&mut self, text: &str) {
        self.editor.paste(text);
        self.update_completion();
    }

    fn update_completion(&mut self) {
        let offer = self
            .completer
            .offer(self.editor.text(), self.editor.cursor());
        self.completion = offer.map(|offer| Completion { offer, selected: 0 });
    }

    /// Takes in a key; `now` is when it was pressed.
    pub fn on_key(&mut self, key: KeyEvent, now: Instant) -> Option<Action> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return self.ctrl_c(now);
        }
        self.ctrl_c = None;
        self.hint = None;
        if ctrl && key.code == KeyCode::Char('d') && self.editor.is_empty() {
            return Some(Action::Quit);
        }
        if let Some(action) = self.completion_key(key) {
            return action;
        }
        match key.code {
            KeyCode::Esc if self.busy() => return Some(Action::Interrupt),
            KeyCode::Esc => return None,
            _ => {}
        }
        match self.editor.key(key) {
            Edit::Submit => return self.submit(),
            Edit::Handled => self.update_completion(),
            Edit::Ignored => {}
        }
        None
    }

    /// Ctrl+C: interrupts a running turn, or clears the input; pressed again within
    /// [`QUIT_WINDOW`], it exits.
    fn ctrl_c(&mut self, now: Instant) -> Option<Action> {
        if self
            .ctrl_c
            .is_some_and(|at| now.duration_since(at) <= QUIT_WINDOW)
        {
            return Some(Action::Quit);
        }
        self.ctrl_c = Some(now);
        self.hint = Some("press Ctrl+C again to exit".into());
        if self.busy() {
            return Some(Action::Interrupt);
        }
        self.editor.clear();
        self.completion = None;
        None
    }

    /// Keys for the completion list, when it is shown: `Some` when the key was used.
    fn completion_key(&mut self, key: KeyEvent) -> Option<Option<Action>> {
        let completion = self.completion.as_mut()?;
        let count = completion.offer.items.len();
        match key.code {
            KeyCode::Up => {
                completion.selected = (completion.selected + count - 1) % count;
            }
            KeyCode::Down => completion.selected = (completion.selected + 1) % count,
            KeyCode::Esc => self.completion = None,
            KeyCode::Tab => self.accept(),
            KeyCode::Enter
                if key.modifiers.is_empty()
                    && completion.offer.items[completion.selected].insert
                        != self.editor.text()[completion.offer.replace.clone()] =>
            {
                self.accept()
            }
            _ => return None,
        }
        Some(None)
    }

    /// Puts the selected completion into the input.
    fn accept(&mut self) {
        let Some(completion) = self.completion.take() else {
            return;
        };
        let item = &completion.offer.items[completion.selected];
        self.editor
            .replace_word(completion.offer.replace.clone(), &item.insert);
        self.update_completion();
    }

    /// Enter: sends the input, runs a built-in command, or says why it cannot.
    fn submit(&mut self) -> Option<Action> {
        if self.editor.expanded().trim().is_empty() {
            return None;
        }
        if self.busy() {
            self.hint = Some("a turn is running: press Esc to interrupt it".into());
            return None;
        }
        self.completion = None;
        let full = self.editor.expanded();
        let width = self.width;
        if let Some(invocation) = parse_invocation(&full) {
            let name = invocation.name.to_string();
            if name == "help" {
                self.editor.submit();
                self.transcript.push_user(&full, width);
                self.help();
                return None;
            }
            if name == "quit" {
                return Some(Action::Quit);
            }
            if let Some((_, when)) = LATER.iter().find(|(n, _)| *n == name) {
                self.editor.submit();
                self.transcript.push_user(&full, width);
                self.transcript.push_note(
                    &format!("/{name} is not available yet: it comes {when}."),
                    width,
                );
                return None;
            }
            if name == "init" || (!is_builtin(&name) && self.host.is_command(&name)) {
                self.editor.submit();
                let prepared = self.host.prepare(&full);
                self.transcript.push_user(&full, width);
                for warning in &prepared.warnings {
                    self.transcript.push_warning(warning, width);
                }
                for note in &prepared.notes {
                    self.transcript.push_note(note, width);
                }
                return self.run(prepared.input);
            }
            if !is_builtin(&name) {
                self.transcript.push_error(
                    &format!(
                        "unknown command /{name}; custom commands are Markdown files in .harness/commands, .claude/commands or .opencode/commands; to send text that starts with /, put a word before it"
                    ),
                    width,
                );
                return None;
            }
        }
        let (shown, full) = self.editor.submit();
        self.transcript.push_user(&shown, width);
        self.run(TurnInput::from(full))
    }

    /// `/help`: the commands and the keys.
    fn help(&mut self) {
        let theme = self.theme();
        let mut lines = vec![Line::from(Span::styled("Commands", theme.bold()))];
        for (name, description) in self.completer.commands() {
            lines.push(Line::from(vec![
                Span::styled(format!("  /{}", sanitize(name)), theme.accent()),
                Span::styled(format!("  {}", sanitize(description)), theme.dim()),
            ]));
        }
        lines.push(Line::from(Span::styled("Keys", theme.bold())));
        for (keys, what) in [
            ("Enter", "send"),
            ("Alt+Enter, Shift+Enter, Ctrl+J", "new line"),
            ("Up, Down", "earlier inputs"),
            ("Tab", "complete a /command or @file"),
            ("Ctrl+O", "expand a collapsed paste"),
            ("Esc", "interrupt the running turn"),
            ("Ctrl+C twice", "exit"),
        ] {
            lines.push(Line::from(vec![
                Span::styled(format!("  {keys}"), theme.accent()),
                Span::styled(format!("  {what}"), theme.dim()),
            ]));
        }
        self.transcript.push_lines(lines, self.width);
    }

    /// The status line.
    fn status(&self) -> Line<'static> {
        let theme = self.theme();
        Line::from(Span::styled(
            format!("{} · {}", sanitize(&self.model), self.mode),
            theme.dim(),
        ))
    }

    /// The live region's lines, at most `rows`, and where the cursor is in them.
    pub fn live(&self, rows: usize) -> (Vec<Line<'static>>, Position) {
        let theme = self.theme();
        let width = self.width;
        let (editor, cursor) = self.editor.render("› ", width, &theme);
        let mut below: Vec<Line<'static>> = Vec::new();
        if let Some(completion) = &self.completion {
            below.extend(complete::render(
                &completion.offer,
                completion.selected,
                width,
                &theme,
            ));
        }
        below.extend(wrap(&self.status(), width, &[], &[]));
        if let Some(hint) = &self.hint {
            below.push(Line::from(Span::styled(sanitize(hint), theme.dim())));
        }
        let fixed = editor.len() + below.len();
        let mut lines = self.transcript.live(width, rows.saturating_sub(fixed + 1));
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        let top = lines.len();
        lines.extend(editor);
        lines.extend(below);
        let skip = lines.len().saturating_sub(rows);
        let cursor = Position::new(
            cursor.x,
            (cursor.y as usize + top).saturating_sub(skip) as u16,
        );
        (lines.split_off(skip), cursor)
    }
}
```

- [ ] **Step 5: The session loop**

Create `crates/harness-tui/src/ui.rs`:

```rust
//! The interactive session: the agent runs in a task of its own, fed turns through a channel,
//! while this side draws the terminal, reads keys, and takes in the agent's events.

use std::{io, time::Instant};

use futures::{Stream, StreamExt};
use harness_core::{agent::Agent, event::AgentEvent, turn::TurnInput};
use ratatui::{backend::Backend, crossterm::event::Event};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::{
    app::{Action, App, Host, Options},
    inline::InlineTerminal,
};

/// Whether the session goes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Quit,
}

/// Work for the task that owns the agent.
enum Job {
    Turn {
        input: TurnInput,
        cancel: CancellationToken,
    },
}

/// The interactive session on a terminal.
pub struct Ui<B: Backend> {
    app: App,
    term: InlineTerminal<B>,
    jobs: Option<mpsc::UnboundedSender<Job>>,
    events: mpsc::UnboundedReceiver<AgentEvent>,
    runner: Option<JoinHandle<()>>,
    /// Cancels the running turn.
    cancel: Option<CancellationToken>,
}

impl<B> Ui<B>
where
    B: Backend,
    B::Error: Send + Sync + 'static,
{
    /// Starts the session: `agent` moves to a task of its own.
    pub fn start(
        agent: Agent,
        host: Box<dyn Host>,
        term: InlineTerminal<B>,
        options: Options,
    ) -> Self {
        let (jobs, mut queue) = mpsc::unbounded_channel::<Job>();
        let (events_tx, events) = mpsc::unbounded_channel();
        let runner = tokio::spawn(async move {
            let mut agent = agent;
            while let Some(job) = queue.recv().await {
                match job {
                    Job::Turn { input, cancel } => {
                        agent.run_turn(input, &events_tx, cancel).await;
                    }
                }
            }
        });
        let width = term.width() as usize;
        Ui {
            app: App::new(options, host, width),
            term,
            jobs: Some(jobs),
            events,
            runner: Some(runner),
            cancel: None,
        }
    }

    pub fn app(&self) -> &App {
        &self.app
    }

    pub fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }

    pub fn terminal(&self) -> &InlineTerminal<B> {
        &self.term
    }

    pub fn terminal_mut(&mut self) -> &mut InlineTerminal<B> {
        &mut self.term
    }

    /// Writes the finished lines into the scrollback and redraws the live region.
    pub fn draw(&mut self) -> io::Result<()> {
        let finished = self.app.transcript.take_finished();
        self.term.insert(&finished)?;
        let rows = self.term.height() as usize;
        let (lines, cursor) = self.app.live(rows);
        let height = lines.len() as u16;
        self.term.draw(height, |area, buf| {
            for (i, line) in lines.iter().enumerate() {
                buf.set_line(area.x, area.y + i as u16, line, area.width);
            }
            Some(ratatui::layout::Position::new(
                area.x + cursor.x,
                area.y + cursor.y,
            ))
        })
    }

    fn dispatch(&mut self, action: Action) -> Flow {
        match action {
            Action::Run(input) => {
                let cancel = CancellationToken::new();
                self.cancel = Some(cancel.clone());
                if let Some(jobs) = &self.jobs {
                    let _ = jobs.send(Job::Turn { input, cancel });
                }
                Flow::Continue
            }
            Action::Interrupt => {
                if let Some(cancel) = &self.cancel {
                    cancel.cancel();
                }
                Flow::Continue
            }
            Action::Quit => Flow::Quit,
        }
    }

    /// Takes in one terminal event: a key, a paste, or a resize.
    pub fn handle(&mut self, event: Event) -> io::Result<Flow> {
        let flow = match event {
            Event::Key(key) => match self.app.on_key(key, Instant::now()) {
                Some(action) => self.dispatch(action),
                None => Flow::Continue,
            },
            Event::Paste(text) => {
                self.app.on_paste(&text);
                Flow::Continue
            }
            Event::Resize(..) => {
                self.term.resized()?;
                self.app.set_width(self.term.width() as usize);
                Flow::Continue
            }
            _ => Flow::Continue,
        };
        if flow == Flow::Continue {
            self.draw()?;
        }
        Ok(flow)
    }

    /// Takes in an event from the agent, and the others already waiting.
    fn agent_event(&mut self, event: AgentEvent) -> io::Result<Flow> {
        self.app.on_event(&event);
        while let Ok(event) = self.events.try_recv() {
            self.app.on_event(&event);
        }
        self.draw()?;
        Ok(Flow::Continue)
    }

    /// Waits for the agent's next event and takes it in.
    pub async fn next(&mut self) -> io::Result<Flow> {
        match self.events.recv().await {
            Some(event) => self.agent_event(event),
            None => Ok(Flow::Quit),
        }
    }

    /// Takes in the agent's events until no turn is running.
    pub async fn settle(&mut self) -> io::Result<()> {
        while self.app.busy() || !self.events.is_empty() {
            if self.next().await? == Flow::Quit {
                break;
            }
        }
        Ok(())
    }

    /// Runs the session on `input`, the terminal's events, until the user leaves.
    pub async fn run<S>(mut self, mut input: S) -> io::Result<()>
    where
        S: Stream<Item = io::Result<Event>> + Unpin,
    {
        self.draw()?;
        loop {
            let flow = tokio::select! {
                event = input.next() => match event {
                    Some(Ok(event)) => self.handle(event)?,
                    Some(Err(e)) => {
                        self.finish().await?;
                        return Err(e);
                    }
                    None => Flow::Quit,
                },
                event = self.events.recv() => match event {
                    Some(event) => self.agent_event(event)?,
                    None => Flow::Quit,
                },
            };
            if flow == Flow::Quit {
                break;
            }
        }
        self.finish().await
    }

    /// Ends the session: stops a running turn, waits for the agent to be dropped (which
    /// releases the session file), writes what finished meanwhile, and clears the live region.
    pub async fn finish(&mut self) -> io::Result<()> {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
        self.jobs = None;
        if let Some(runner) = self.runner.take() {
            let _ = runner.await;
        }
        while let Ok(event) = self.events.try_recv() {
            self.app.on_event(&event);
        }
        let finished = self.app.transcript.take_finished();
        self.term.insert(&finished)?;
        self.term.clear()
    }
}
```

In `crates/harness-tui/src/lib.rs`:

Replace (1 of 2):

```rust
//! only a small live region at the bottom (the input, what is streaming, prompts) is redrawn.
//! This crate draws with `ratatui`; `harness-cli` sets up the session and starts it.

pub mod complete;
pub mod diff;
pub mod editor;
```

with:

```rust
//! only a small live region at the bottom (the input, what is streaming, prompts) is redrawn.
//! This crate draws with `ratatui`; `harness-cli` sets up the session and starts it.

pub mod app;
pub mod complete;
pub mod diff;
pub mod editor;
```

Replace (2 of 2):

```rust
pub mod inline;
pub mod markdown;
pub mod style;
pub mod text;
pub mod transcript;
```

with:

```rust
pub mod inline;
pub mod markdown;
pub mod style;
pub mod terminal;
pub mod text;
pub mod transcript;
pub mod ui;
```

Run: `cargo test -p harness-tui`
Expected: PASS, including `a_prompt_runs_a_turn_and_the_reply_goes_into_the_scrollback` and `the_terminal_modes_are_set_and_undone_in_reverse_order`.

- [ ] **Step 6: The interactive base prompt, and slash commands that return their messages**

In `crates/harness-cli/src/prompt.rs`:

Replace (1 of 2):

```rust
use harness_core::permission::Mode;

/// The base system prompt: who the agent is and what the approval mode and sandbox let it do.
/// Kept short on purpose, since local models have small context windows. The instruction files and
/// the environment follow it (see `context::system_prompt`).
pub fn base_prompt(mode: Mode, sandboxed: bool) -> String {
    let sandbox_line = if !sandboxed && mode == Mode::FullAccess {
        "No OS sandbox is active."
    } else if !sandboxed && matches!(mode, Mode::Plan | Mode::ReadOnly) {
        "No OS sandbox is active, so shell commands and file edits are refused; use the read, grep and glob tools instead."
    } else if !sandboxed {
        "No OS sandbox is active, so every shell command needs approval and will be refused; use the file tools instead."
    } else if matches!(mode, Mode::Plan | Mode::ReadOnly) {
```

with:

```rust
use harness_core::permission::Mode;

/// The base system prompt: who the agent is and what the approval mode and sandbox let it do, and
/// whether a user can answer approvals (`interactive`). Kept short on purpose, since local models
/// have small context windows. The instruction files and the environment follow it (see
/// `context::system_prompt`).
pub fn base_prompt(mode: Mode, sandboxed: bool, interactive: bool) -> String {
    let sandbox_line = if !sandboxed && mode == Mode::FullAccess {
        "No OS sandbox is active."
    } else if !sandboxed && matches!(mode, Mode::Plan | Mode::ReadOnly) {
        "No OS sandbox is active, so shell commands and file edits are refused; use the read, grep and glob tools instead."
    } else if !sandboxed && interactive {
        "No OS sandbox is active, so every shell command needs the user's approval."
    } else if !sandboxed {
        "No OS sandbox is active, so every shell command needs approval and will be refused; use the file tools instead."
    } else if matches!(mode, Mode::Plan | Mode::ReadOnly) {
```

Replace (2 of 2):

```rust
    };
    let approval_line = if mode == Mode::FullAccess {
        "In full-access mode actions run without approval, except those a deny rule forbids or may match."
    } else {
        "You are running non-interactively, so actions that need approval will be refused, including destructive commands, commands harness cannot fully analyse, commands the sandbox blocks, anything a rule forbids or asks to confirm, and in ask mode any unlisted command or file edit."
    };
```

with:

```rust
    };
    let approval_line = if mode == Mode::FullAccess {
        "In full-access mode actions run without approval, except those a deny rule forbids or may match."
    } else if interactive {
        "The user is at the terminal: actions that need approval, such as destructive commands, commands harness cannot fully analyse, anything a rule asks to confirm, and in ask mode any unlisted command or file edit, are shown to them to approve or deny. A denied action's result says so, sometimes with the user's reason."
    } else {
        "You are running non-interactively, so actions that need approval will be refused, including destructive commands, commands harness cannot fully analyse, commands the sandbox blocks, anything a rule forbids or asks to confirm, and in ask mode any unlisted command or file edit."
    };
```

Nothing may write to the terminal in raw mode, so expanding a command returns its warnings and notes, and `harness ask` prints them. In `crates/harness-cli/src/slash.rs`:

Replace (1 of 6):

```rust
    }
}

/// The turn for `prompt` followed by `piped` (the piped-stdin text appended to it, possibly
/// empty). `/init` and custom commands are expanded and `piped` added after them; anything else is
/// sent as it is.
```

with:

```rust
    }
}

/// Something to tell the user about expanding a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Warning(String),
    Note(String),
}

/// A turn's input, and what to tell the user about how it was expanded, in order.
pub struct Expanded {
    pub input: TurnInput,
    pub messages: Vec<Message>,
}

/// Prints `messages` to stderr, escaped.
pub fn print_messages(messages: &[Message]) {
    for message in messages {
        match message {
            Message::Warning(text) => eprintln!("warning: {}", terminal_safe(text)),
            Message::Note(text) => eprintln!("note: {}", terminal_safe(text)),
        }
    }
}

/// The turn for `prompt` followed by `piped` (the piped-stdin text appended to it, possibly
/// empty). `/init` and custom commands are expanded and `piped` added after them; anything else is
/// sent as it is.
```

Replace (2 of 6):

```rust
    commands: Option<&Commands>,
    setup: &Setup,
    policy: &dyn PermissionPolicy,
) -> TurnInput {
    let whole = || TurnInput::from(format!("{prompt}{piped}"));
    let (Some(invocation), Some(commands)) = (parse_invocation(prompt), commands) else {
        return whole();
    };
```

with:

```rust
    commands: Option<&Commands>,
    setup: &Setup,
    policy: &dyn PermissionPolicy,
) -> Expanded {
    let whole = || Expanded {
        input: TurnInput::from(format!("{prompt}{piped}")),
        messages: Vec::new(),
    };
    let (Some(invocation), Some(commands)) = (parse_invocation(prompt), commands) else {
        return whole();
    };
```

Replace (3 of 6):

```rust
        if !piped.is_empty() {
            input.parts.push(InputPart::Text(piped.to_string()));
        }
        return input;
    }
    let Some(command) = commands.get(invocation.name) else {
        return whole();
```

with:

```rust
        if !piped.is_empty() {
            input.parts.push(InputPart::Text(piped.to_string()));
        }
        return Expanded {
            input,
            messages: Vec::new(),
        };
    }
    let Some(command) = commands.get(invocation.name) else {
        return whole();
```

Replace (4 of 6):

```rust
        trusted: config::is_trusted(&setup.paths.global_config_file(), &root, &setup.trust),
    };
    let expansion = expand(command, invocation.args, &setup.workspace, policy, trust);
    for warning in &expansion.warnings {
        eprintln!("warning: {}", terminal_safe(warning));
    }
    for note in &expansion.notes {
        eprintln!("note: {}", terminal_safe(note));
    }
    let mut input = expansion.input;
    if !piped.is_empty() {
        input.parts.push(InputPart::Text(piped.to_string()));
```

with:

```rust
        trusted: config::is_trusted(&setup.paths.global_config_file(), &root, &setup.trust),
    };
    let expansion = expand(command, invocation.args, &setup.workspace, policy, trust);
    let mut messages: Vec<Message> = expansion
        .warnings
        .iter()
        .cloned()
        .map(Message::Warning)
        .collect();
    messages.extend(expansion.notes.iter().cloned().map(Message::Note));
    let mut input = expansion.input;
    if !piped.is_empty() {
        input.parts.push(InputPart::Text(piped.to_string()));
```

Replace (5 of 6):

```rust
    if let Some(model) = expansion.model {
        match registry::resolve(&model, &setup.config.providers, setup::env) {
            Ok(resolved) => {
                eprintln!("{}", runs_on(&command.name, &resolved.id));
                input.model = Some(TurnModel {
                    provider: resolved.provider,
                    id: resolved.id,
                    name: resolved.model,
                });
            }
            Err(e) => eprintln!("{}", cannot_use(&command.name, &model, &e.to_string())),
        }
    }
    input
}

/// The note that command `name` runs on `model`.
fn runs_on(name: &str, model: &str) -> String {
    format!(
        "note: /{} runs on {}, as its command file asks",
        terminal_safe(name),
        terminal_safe(model)
    )
}

/// The warning that command `name` asks for `model`, which `error` keeps from being used.
fn cannot_use(name: &str, model: &str, error: &str) -> String {
    format!(
        "warning: /{} asks for model {}, which cannot be used ({}); using the session's model",
        terminal_safe(name),
        terminal_safe(model),
        terminal_safe(error)
    )
}

```

with:

```rust
    if let Some(model) = expansion.model {
        match registry::resolve(&model, &setup.config.providers, setup::env) {
            Ok(resolved) => {
                messages.push(Message::Note(runs_on(&command.name, &resolved.id)));
                input.model = Some(TurnModel {
                    provider: resolved.provider,
                    id: resolved.id,
                    name: resolved.model,
                });
            }
            Err(e) => messages.push(Message::Warning(cannot_use(
                &command.name,
                &model,
                &e.to_string(),
            ))),
        }
    }
    Expanded { input, messages }
}

/// The note that command `name` runs on `model`.
fn runs_on(name: &str, model: &str) -> String {
    format!("/{name} runs on {model}, as its command file asks")
}

/// The warning that command `name` asks for `model`, which `error` keeps from being used.
fn cannot_use(name: &str, model: &str, error: &str) -> String {
    format!(
        "/{name} asks for model {model}, which cannot be used ({error}); using the session's model"
    )
}

```

Replace (6 of 6):

```rust
mod tests {
    use super::*;

    // Review C, minor 7: a command's name is printed like any other text from a file.
    #[test]
    fn model_messages_print_the_command_name_safely() {
        let name = "x\u{1b}[2J";
        for message in [
            runs_on(name, "mock/m"),
            cannot_use(name, "mock/m", "unknown provider"),
        ] {
            assert!(!message.contains('\u{1b}'), "{message:?}");
            assert!(message.contains("/x\\u{1b}[2J"), "{message:?}");
        }
    }
}
```

with:

```rust
mod tests {
    use super::*;

    // Review C, minor 7: a command's name is printed like any other text from a file: the
    // messages carry it as it is, and are escaped where they are printed.
    #[test]
    fn model_messages_name_the_command_and_are_printed_safely() {
        let name = "x\u{1b}[2J";
        for message in [
            runs_on(name, "mock/m"),
            cannot_use(name, "mock/m", "unknown provider"),
        ] {
            assert!(message.contains("/x\u{1b}[2J"), "{message:?}");
            let printed = terminal_safe(&message);
            assert!(!printed.contains('\u{1b}'), "{printed:?}");
            assert!(printed.contains("/x\\u{1b}[2J"), "{printed:?}");
        }
    }
}
```

- [ ] **Step 7: One startup for `ask` and the interactive session**

The agent's setup moves out of `ask.rs`, with its tests.

Create `crates/harness-cli/src/start.rs`:

```rust
//! Starting a session's agent, as `harness ask` and the interactive terminal both do: the
//! sandbox and its warnings, the permission engine, the tools' context, the system prompt, the
//! checkpoints, and the agent itself.

use std::{
    io::Write,
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use harness_config::config::LinuxGitProtection;
use harness_core::{
    agent::{Agent, AgentConfig, Approver},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    permission::{FsAccess, Mode},
    session::Session,
    tokens::DEFAULT_CONTEXT_WINDOW,
    tool::{CommandSandbox, ToolContext},
};
use harness_providers::registry::Resolved;

use crate::{
    prompt, sandbox,
    setup::Setup,
    term::{terminal_safe, terminal_safe_text},
};

/// What a frontend asks for.
pub struct Request<'a> {
    pub setup: &'a Setup,
    pub mode: Mode,
    pub model: Resolved,
    pub session: Session,
    pub approver: Arc<dyn Approver>,
    /// Whether a user at a terminal answers approvals.
    pub interactive: bool,
}

/// A started agent, and what the frontend needs next to it.
pub struct Started {
    pub agent: Agent,
    /// Ends the sandbox's session; drop the agent first.
    pub sandbox_session: SessionEnd,
    pub policy: Arc<PermissionEngine>,
}

/// The context window of model `id`, in tokens. Model profiles (P4) replace this assumption.
pub fn context_window(_id: &str) -> u64 {
    DEFAULT_CONTEXT_WINDOW
}

/// Starts the agent for `request`, printing startup warnings to stderr.
pub async fn start(request: Request<'_>) -> Started {
    let Request {
        setup,
        mode,
        model: resolved,
        session,
        approver,
        interactive,
    } = request;
    if mode == Mode::FullAccess {
        eprintln!("warning: full-access mode: commands run without approval or sandbox");
    }
    let run_id = format!(
        "run-{}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        std::process::id()
    );
    let output_dir = setup.paths.state_dir.join("tool-output").join(run_id);
    let sandbox_disabled_by_env =
        std::env::var("HARNESS_SANDBOX").as_deref() == Ok("none") && mode != Mode::FullAccess;
    // Write access to `/`, `$HOME` or an ancestor of it would cover the user's dotfiles. Plan and
    // read-only modes keep their read-only sandbox.
    let workspace_too_broad = mode.fs_access() == FsAccess::WorkspaceWrite
        && harness_sandbox::workspace_is_too_broad(&setup.workspace);
    let required = setup.config.linux_git_protection == LinuxGitProtection::Required;
    let settings = harness_sandbox::SandboxSettings {
        extra_writable: setup.config.writable_roots.clone(),
        allow_localhost: setup.config.allow_localhost,
        quarantine_dir: Some(setup.paths.data_dir.join("quarantine")),
        require_full_git_protection: required,
    };
    let detected = if mode == Mode::FullAccess || sandbox_disabled_by_env || workspace_too_broad {
        None
    } else {
        harness_sandbox::detect(settings.clone())
    };
    let choice = sandbox::choose(detected, mode.fs_access(), required);
    if let Some(warning) = &choice.warning {
        eprintln!("warning: {}", terminal_safe(warning));
    }
    let sandbox = choice.sandbox;
    // From here on, however the run is left, the sandbox's session ends: on Linux that ends what
    // sandboxed commands left running, and checks git metadata once more.
    let sandbox_session = SessionEnd::new(sandbox.clone());
    let sandboxed = sandbox.is_some();
    if mode != Mode::FullAccess && !sandboxed && choice.warning.is_none() {
        if sandbox_disabled_by_env {
            eprintln!(
                "warning: the sandbox is disabled by HARNESS_SANDBOX=none; every shell command will need approval"
            );
        } else if workspace_too_broad {
            eprintln!(
                "warning: the workspace {} is your home directory or above, where the sandbox would make your dotfiles writable, so it is off; every shell command will need approval",
                terminal_safe(&setup.workspace.display().to_string())
            );
        } else {
            eprintln!(
                "warning: no OS sandbox is available; every shell command will need approval"
            );
        }
    }
    let mut read_dirs = setup.config.read_dirs.clone();
    read_dirs.push(output_dir.clone());
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode,
        workspace: setup.workspace.clone(),
        read_dirs,
        rules: RuleSet {
            allow: setup.config.allow.clone(),
            deny: setup.config.deny.clone(),
            confirm: setup.config.confirm.clone(),
        },
        sandbox_available: sandboxed,
        writes_need_approval: workspace_too_broad,
    }));
    for rule in policy.unknown_rules() {
        eprintln!(
            "warning: rule `{}` names an unknown tool (use bash:, read:, or write:)",
            terminal_safe(&rule)
        );
    }
    let ctx = tool_context(&setup.workspace, sandbox, mode.fs_access()).await;
    let mut config = AgentConfig::new(
        resolved.id.clone(),
        resolved.model.clone(),
        crate::context::system_prompt(setup, &prompt::base_prompt(mode, sandboxed, interactive)),
        output_dir,
    );
    config.context_window = context_window(&resolved.id);
    if let Some(steps) = setup.config.max_steps {
        config.max_steps = steps;
    }
    config.compaction = harness_core::compaction::CompactionConfig {
        threshold: setup.config.compaction.threshold(),
        keep_recent: setup.config.compaction.keep_recent(),
    };
    // Sandboxed commands run without approval: what they can write to must not hold the
    // checkpoint repository, which harness's own git reads outside the sandbox.
    let writable = if sandboxed {
        harness_sandbox::writable_roots(&settings, &setup.workspace)
    } else {
        Vec::new()
    };
    let checkpoints = crate::sessions::checkpoints(setup, &session, &writable);
    let agent = Agent::new(
        resolved.provider,
        harness_tools::builtin(),
        policy.clone(),
        approver,
        config,
        ctx,
    )
    .with_session(session)
    .with_checkpoints(checkpoints);
    Started {
        agent,
        sandbox_session,
        policy,
    }
}

/// Ends the run: first the agent, which releases the session file, then the sandbox's session,
/// which can take a few seconds, so a `-c` started once the answer prints finds the file free.
pub fn end_run(agent: Agent, sandbox_session: SessionEnd) {
    drop(agent);
    sandbox_session.end();
}

/// Ends the sandbox's session ([`CommandSandbox::end_session`]) once: when [`end`](Self::end) is
/// called, or when dropped, so that an early return or a panic ends it too. What that says goes
/// to stderr, escaped; the exit code does not depend on it. It takes a few seconds at most.
pub struct SessionEnd(Option<Arc<dyn CommandSandbox>>);

impl SessionEnd {
    pub fn new(sandbox: Option<Arc<dyn CommandSandbox>>) -> SessionEnd {
        SessionEnd(sandbox)
    }

    /// Ends the session now.
    pub fn end(mut self) {
        self.run();
    }

    fn run(&mut self) {
        let Some(sandbox) = self.0.take() else {
            return;
        };
        if let Some(text) = sandbox.end_session() {
            let _ = write!(std::io::stderr().lock(), "{}", terminal_safe_text(&text));
        }
    }
}

impl Drop for SessionEnd {
    fn drop(&mut self) {
        self.run();
    }
}

/// The tools' context, with the sandbox's session started first: before the agent runs, so what
/// the sandbox reads from the workspace (on Linux, the ignore rules the git-metadata guard scans
/// with) is what was there before any tool could change it.
pub async fn tool_context(
    workspace: &Path,
    sandbox: Option<Arc<dyn CommandSandbox>>,
    access: FsAccess,
) -> ToolContext {
    let ctx = ToolContext::new(workspace).with_sandbox(sandbox, access);
    if let Some(sandbox) = ctx.sandbox.clone() {
        let workspace = ctx.workspace.clone();
        // It may walk the whole workspace. Should it fail, the first command reads what it needs.
        let _ = tokio::task::spawn_blocking(move || sandbox.start_session(&workspace)).await;
    }
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use harness_core::{agent::NonInteractive, tool::Tool};

    /// Runs commands directly, and records what harness asks of it.
    #[derive(Debug, Default)]
    struct Recording {
        log: Mutex<Vec<String>>,
    }

    impl Recording {
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    impl CommandSandbox for Recording {
        fn name(&self) -> &'static str {
            "recording"
        }

        fn command(
            &self,
            _access: FsAccess,
            _workspace: &Path,
            program: &str,
            args: &[&str],
        ) -> std::io::Result<tokio::process::Command> {
            self.log.lock().unwrap().push("command".into());
            let mut cmd = tokio::process::Command::new(program);
            cmd.args(args).process_group(0);
            Ok(cmd)
        }

        fn is_denial(&self, _exit_code: Option<i32>, _output: &str) -> bool {
            false
        }

        fn start_session(&self, workspace: &Path) {
            self.log
                .lock()
                .unwrap()
                .push(format!("start_session {}", workspace.display()));
        }

        fn end_session(&self) -> Option<String> {
            self.log.lock().unwrap().push("end_session".into());
            Some("ended\n".into())
        }
    }

    fn ended(sandbox: &Recording) -> usize {
        sandbox
            .log()
            .iter()
            .filter(|entry| *entry == "end_session")
            .count()
    }

    #[test]
    fn the_sandbox_session_ends_once_when_the_turn_ends() {
        let sandbox = Arc::new(Recording::default());
        let shared: Arc<dyn CommandSandbox> = sandbox.clone();
        let session = SessionEnd::new(Some(shared));
        assert_eq!(ended(&sandbox), 0);
        session.end();
        assert_eq!(ended(&sandbox), 1);
    }

    #[test]
    fn the_sandbox_session_ends_however_run_is_left() {
        // An early return drops the guard; so does a panic.
        let sandbox = Arc::new(Recording::default());
        let shared: Arc<dyn CommandSandbox> = sandbox.clone();
        drop(SessionEnd::new(Some(shared)));
        assert_eq!(ended(&sandbox), 1);
        let shared: Arc<dyn CommandSandbox> = sandbox.clone();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _session = SessionEnd::new(Some(shared));
            panic!("the turn failed");
        }));
        assert!(unwound.is_err());
        assert_eq!(ended(&sandbox), 2);
        // Without a sandbox there is nothing to end.
        SessionEnd::new(None).end();
    }

    #[tokio::test]
    async fn the_sandbox_session_starts_before_the_first_tool_call() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Arc::new(Recording::default());
        let shared: Arc<dyn CommandSandbox> = sandbox.clone();
        let ctx = tool_context(dir.path(), Some(shared), FsAccess::WorkspaceWrite).await;
        let started = format!("start_session {}", ctx.workspace.display());
        assert_eq!(sandbox.log(), [started.as_str()]);
        let out = harness_tools::BashTool
            .run(serde_json::json!({"command": "echo hi"}), &ctx)
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(sandbox.log(), [started.as_str(), "command"]);
    }

    #[tokio::test]
    async fn without_a_sandbox_there_is_no_session_to_start() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = tool_context(dir.path(), None, FsAccess::WorkspaceWrite).await;
        assert!(ctx.sandbox.is_none());
    }

    /// Records, when its session ends, whether the harness session file at `path` could be
    /// opened then.
    #[derive(Debug)]
    struct LockProbe {
        path: std::path::PathBuf,
        free: Mutex<Option<bool>>,
    }

    impl CommandSandbox for LockProbe {
        fn name(&self) -> &'static str {
            "lock probe"
        }

        fn command(
            &self,
            _access: FsAccess,
            _workspace: &Path,
            program: &str,
            _args: &[&str],
        ) -> std::io::Result<tokio::process::Command> {
            Ok(tokio::process::Command::new(program))
        }

        fn is_denial(&self, _exit_code: Option<i32>, _output: &str) -> bool {
            false
        }

        fn start_session(&self, _workspace: &Path) {}

        fn end_session(&self) -> Option<String> {
            let free = harness_core::session::Session::open(&self.path).is_ok();
            *self.free.lock().unwrap() = Some(free);
            None
        }
    }

    // Review D M9: ending the sandbox's session takes a few seconds on Linux; the session file
    // is released before, so a `-c` started as soon as the answer prints can use it.
    #[test]
    fn the_session_is_released_before_the_sandbox_session_ends() {
        use harness_core::{
            message::Message,
            session::{EntryKind, Session},
            testing::MockProvider,
            tool::ToolRegistry,
        };
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(&dir.path().join("sessions"), dir.path());
        session.append(EntryKind::Message {
            message: Message::User {
                content: "hi".into(),
            },
            display: None,
            note: false,
        });
        let path = session.path().unwrap().to_path_buf();
        let policy = Arc::new(PermissionEngine::new(EngineConfig {
            mode: Mode::Auto,
            workspace: dir.path().to_path_buf(),
            read_dirs: vec![],
            rules: RuleSet::default(),
            sandbox_available: false,
            writes_need_approval: false,
        }));
        let agent = Agent::new(
            MockProvider::new(vec![]),
            ToolRegistry::new(vec![]),
            policy,
            Arc::new(NonInteractive),
            AgentConfig::new("mock/m", "m", "system", dir.path().join("out")),
            ToolContext::new(dir.path()),
        )
        .with_session(session);
        let probe = Arc::new(LockProbe {
            path,
            free: Mutex::new(None),
        });
        let shared: Arc<dyn CommandSandbox> = probe.clone();
        end_run(agent, SessionEnd::new(Some(shared)));
        assert_eq!(*probe.free.lock().unwrap(), Some(true));
    }
}
```

In `crates/harness-cli/src/ask.rs`:

Replace (1 of 5):

```rust
use std::{
    io::{IsTerminal, Read, Write},
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use harness_config::config::{self, LinuxGitProtection};
use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    event::{AgentEvent, TurnEndReason},
    permission::{FsAccess, Mode},
    tool::{CommandSandbox, ToolContext},
};
use harness_providers::registry;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    models, prompt, sandbox, setup,
    term::{terminal_safe, terminal_safe_text},
};

```

with:

```rust
use std::{
    io::{IsTerminal, Read, Write},
    sync::Arc,
    time::Duration,
};

use harness_config::config;
use harness_core::{
    agent::NonInteractive,
    event::{AgentEvent, TurnEndReason},
    permission::Mode,
};
use harness_providers::registry;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    models, setup,
    start::{self, Request, Started, end_run},
    term::{terminal_safe, terminal_safe_text},
};

```

Replace (2 of 5):

```rust
    let mode = mode_flag
        .or(setup.config.mode)
        .unwrap_or_else(|| config::default_mode(&setup.workspace));
    if mode == Mode::FullAccess {
        eprintln!("warning: full-access mode: commands run without approval or sandbox");
    }
    let run_id = format!(
        "run-{}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        std::process::id()
    );
    let output_dir = setup.paths.state_dir.join("tool-output").join(run_id);
    let sandbox_disabled_by_env =
        std::env::var("HARNESS_SANDBOX").as_deref() == Ok("none") && mode != Mode::FullAccess;
    // Write access to `/`, `$HOME` or an ancestor of it would cover the user's dotfiles. Plan and
    // read-only modes keep their read-only sandbox.
    let workspace_too_broad = mode.fs_access() == FsAccess::WorkspaceWrite
        && harness_sandbox::workspace_is_too_broad(&setup.workspace);
    let required = setup.config.linux_git_protection == LinuxGitProtection::Required;
    let settings = harness_sandbox::SandboxSettings {
        extra_writable: setup.config.writable_roots.clone(),
        allow_localhost: setup.config.allow_localhost,
        quarantine_dir: Some(setup.paths.data_dir.join("quarantine")),
        require_full_git_protection: required,
    };
    let detected = if mode == Mode::FullAccess || sandbox_disabled_by_env || workspace_too_broad {
        None
    } else {
        harness_sandbox::detect(settings.clone())
    };
    let choice = sandbox::choose(detected, mode.fs_access(), required);
    if let Some(warning) = &choice.warning {
        eprintln!("warning: {}", terminal_safe(warning));
    }
    let sandbox = choice.sandbox;
    // From here on, however `run` is left, the sandbox's session ends: on Linux that ends what
    // sandboxed commands left running, and checks git metadata once more.
    let sandbox_session = SessionEnd::new(sandbox.clone());
    let sandboxed = sandbox.is_some();
    if mode != Mode::FullAccess && !sandboxed && choice.warning.is_none() {
        if sandbox_disabled_by_env {
            eprintln!(
                "warning: the sandbox is disabled by HARNESS_SANDBOX=none; every shell command will need approval"
            );
        } else if workspace_too_broad {
            eprintln!(
                "warning: the workspace {} is your home directory or above, where the sandbox would make your dotfiles writable, so it is off; every shell command will need approval",
                terminal_safe(&setup.workspace.display().to_string())
            );
        } else {
            eprintln!(
                "warning: no OS sandbox is available; every shell command will need approval"
            );
        }
    }
    let mut read_dirs = setup.config.read_dirs.clone();
    read_dirs.push(output_dir.clone());
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode,
        workspace: setup.workspace.clone(),
        read_dirs,
        rules: RuleSet {
            allow: setup.config.allow.clone(),
            deny: setup.config.deny.clone(),
            confirm: setup.config.confirm.clone(),
        },
        sandbox_available: sandboxed,
        writes_need_approval: workspace_too_broad,
    }));
    for rule in policy.unknown_rules() {
        eprintln!(
            "warning: rule `{}` names an unknown tool (use bash:, read:, or write:)",
            terminal_safe(&rule)
        );
    }
    let ctx = tool_context(&setup.workspace, sandbox, mode.fs_access()).await;
    let mut config = AgentConfig::new(
        resolved.id.clone(),
        resolved.model.clone(),
        crate::context::system_prompt(&setup, &prompt::base_prompt(mode, sandboxed)),
        output_dir,
    );
    if let Some(steps) = setup.config.max_steps {
        config.max_steps = steps;
    }
    config.compaction = harness_core::compaction::CompactionConfig {
        threshold: setup.config.compaction.threshold(),
        keep_recent: setup.config.compaction.keep_recent(),
    };
    // `with_piped_stdin` returns the prompt with any piped text appended.
    let turn = crate::slash::turn_input(
        &typed,
```

with:

```rust
    let mode = mode_flag
        .or(setup.config.mode)
        .unwrap_or_else(|| config::default_mode(&setup.workspace));
    let Started {
        mut agent,
        sandbox_session,
        policy,
    } = start::start(Request {
        setup: &setup,
        mode,
        model: resolved,
        session,
        approver: Arc::new(NonInteractive),
        interactive: false,
    })
    .await;
    // `with_piped_stdin` returns the prompt with any piped text appended.
    let turn = crate::slash::turn_input(
        &typed,
```

Replace (3 of 5):

```rust
        &setup,
        &*policy,
    );
    // Sandboxed commands run without approval: what they can write to must not hold the
    // checkpoint repository, which harness's own git reads outside the sandbox.
    let writable = if sandboxed {
        harness_sandbox::writable_roots(&settings, &setup.workspace)
    } else {
        Vec::new()
    };
    let checkpoints = crate::sessions::checkpoints(&setup, &session, &writable);
    let mut agent = Agent::new(
        resolved.provider,
        harness_tools::builtin(),
        policy,
        Arc::new(NonInteractive),
        config,
        ctx,
    )
    .with_session(session)
    .with_checkpoints(checkpoints);

    let (tx, rx) = mpsc::unbounded_channel();
    let renderer = tokio::spawn(render(rx, json, cancel.clone()));
```

with:

```rust
        &setup,
        &*policy,
    );
    crate::slash::print_messages(&turn.messages);
    let turn = turn.input;

    let (tx, rx) = mpsc::unbounded_channel();
    let renderer = tokio::spawn(render(rx, json, cancel.clone()));
```

Replace (4 of 5):

```rust
    }
    end_run(agent, sandbox_session);
    exit_code(reason, blocked)
}

/// Ends the run: first the agent, which releases the session file, then the sandbox's session,
/// which can take a few seconds, so a `-c` started once the answer prints finds the file free.
fn end_run(agent: Agent, sandbox_session: SessionEnd) {
    drop(agent);
    sandbox_session.end();
}

/// Ends the sandbox's session ([`CommandSandbox::end_session`]) once: when [`end`](Self::end) is
/// called, or when dropped, so that an early return or a panic ends it too. What that says goes
/// to stderr, escaped; the exit code does not depend on it. It takes a few seconds at most.
struct SessionEnd(Option<Arc<dyn CommandSandbox>>);

impl SessionEnd {
    fn new(sandbox: Option<Arc<dyn CommandSandbox>>) -> SessionEnd {
        SessionEnd(sandbox)
    }

    /// Ends the session now.
    fn end(mut self) {
        self.run();
    }

    fn run(&mut self) {
        let Some(sandbox) = self.0.take() else {
            return;
        };
        if let Some(text) = sandbox.end_session() {
            let _ = write!(std::io::stderr().lock(), "{}", terminal_safe_text(&text));
        }
    }
}

impl Drop for SessionEnd {
    fn drop(&mut self) {
        self.run();
    }
}

/// The tools' context, with the sandbox's session started first: before the agent runs, so what
/// the sandbox reads from the workspace (on Linux, the ignore rules the git-metadata guard scans
/// with) is what was there before any tool could change it.
async fn tool_context(
    workspace: &Path,
    sandbox: Option<Arc<dyn CommandSandbox>>,
    access: FsAccess,
) -> ToolContext {
    let ctx = ToolContext::new(workspace).with_sandbox(sandbox, access);
    if let Some(sandbox) = ctx.sandbox.clone() {
        let workspace = ctx.workspace.clone();
        // It may walk the whole workspace. Should it fail, the first command reads what it needs.
        let _ = tokio::task::spawn_blocking(move || sandbox.start_session(&workspace)).await;
    }
    ctx
}

/// Maps how the turn ended to the documented exit codes.
```

with:

```rust
    }
    end_run(agent, sandbox_session);
    exit_code(reason, blocked)
}

/// Maps how the turn ended to the documented exit codes.
```

Replace (5 of 5):

```rust
mod tests {
    use super::*;

    use std::sync::Mutex;

    use harness_core::tool::Tool;

    /// Runs commands directly, and records what harness asks of it.
    #[derive(Debug, Default)]
    struct Recording {
        log: Mutex<Vec<String>>,
    }

    impl Recording {
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    impl CommandSandbox for Recording {
        fn name(&self) -> &'static str {
            "recording"
        }

        fn command(
            &self,
            _access: FsAccess,
            _workspace: &Path,
            program: &str,
            args: &[&str],
        ) -> std::io::Result<tokio::process::Command> {
            self.log.lock().unwrap().push("command".into());
            let mut cmd = tokio::process::Command::new(program);
            cmd.args(args).process_group(0);
            Ok(cmd)
        }

        fn is_denial(&self, _exit_code: Option<i32>, _output: &str) -> bool {
            false
        }

        fn start_session(&self, workspace: &Path) {
            self.log
                .lock()
                .unwrap()
                .push(format!("start_session {}", workspace.display()));
        }

        fn end_session(&self) -> Option<String> {
            self.log.lock().unwrap().push("end_session".into());
            Some("ended\n".into())
        }
    }

    fn ended(sandbox: &Recording) -> usize {
        sandbox
            .log()
            .iter()
            .filter(|entry| *entry == "end_session")
            .count()
    }

    #[test]
    fn the_sandbox_session_ends_once_when_the_turn_ends() {
        let sandbox = Arc::new(Recording::default());
        let shared: Arc<dyn CommandSandbox> = sandbox.clone();
        let session = SessionEnd::new(Some(shared));
        assert_eq!(ended(&sandbox), 0);
        session.end();
        assert_eq!(ended(&sandbox), 1);
    }

    #[test]
    fn the_sandbox_session_ends_however_run_is_left() {
        // An early return drops the guard; so does a panic.
        let sandbox = Arc::new(Recording::default());
        let shared: Arc<dyn CommandSandbox> = sandbox.clone();
        drop(SessionEnd::new(Some(shared)));
        assert_eq!(ended(&sandbox), 1);
        let shared: Arc<dyn CommandSandbox> = sandbox.clone();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _session = SessionEnd::new(Some(shared));
            panic!("the turn failed");
        }));
        assert!(unwound.is_err());
        assert_eq!(ended(&sandbox), 2);
        // Without a sandbox there is nothing to end.
        SessionEnd::new(None).end();
    }

    #[tokio::test]
    async fn the_sandbox_session_starts_before_the_first_tool_call() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Arc::new(Recording::default());
        let shared: Arc<dyn CommandSandbox> = sandbox.clone();
        let ctx = tool_context(dir.path(), Some(shared), FsAccess::WorkspaceWrite).await;
        let started = format!("start_session {}", ctx.workspace.display());
        assert_eq!(sandbox.log(), [started.as_str()]);
        let out = harness_tools::BashTool
            .run(serde_json::json!({"command": "echo hi"}), &ctx)
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(sandbox.log(), [started.as_str(), "command"]);
    }

    #[tokio::test]
    async fn without_a_sandbox_there_is_no_session_to_start() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = tool_context(dir.path(), None, FsAccess::WorkspaceWrite).await;
        assert!(ctx.sandbox.is_none());
    }

    /// Records, when its session ends, whether the harness session file at `path` could be
    /// opened then.
    #[derive(Debug)]
    struct LockProbe {
        path: std::path::PathBuf,
        free: Mutex<Option<bool>>,
    }

    impl CommandSandbox for LockProbe {
        fn name(&self) -> &'static str {
            "lock probe"
        }

        fn command(
            &self,
            _access: FsAccess,
            _workspace: &Path,
            program: &str,
            _args: &[&str],
        ) -> std::io::Result<tokio::process::Command> {
            Ok(tokio::process::Command::new(program))
        }

        fn is_denial(&self, _exit_code: Option<i32>, _output: &str) -> bool {
            false
        }

        fn start_session(&self, _workspace: &Path) {}

        fn end_session(&self) -> Option<String> {
            let free = harness_core::session::Session::open(&self.path).is_ok();
            *self.free.lock().unwrap() = Some(free);
            None
        }
    }

    // Review D M9: ending the sandbox's session takes a few seconds on Linux; the session file
    // is released before, so a `-c` started as soon as the answer prints can use it.
    #[test]
    fn the_session_is_released_before_the_sandbox_session_ends() {
        use harness_core::{
            message::Message,
            session::{EntryKind, Session},
            testing::MockProvider,
            tool::ToolRegistry,
        };
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(&dir.path().join("sessions"), dir.path());
        session.append(EntryKind::Message {
            message: Message::User {
                content: "hi".into(),
            },
            display: None,
            note: false,
        });
        let path = session.path().unwrap().to_path_buf();
        let policy = Arc::new(PermissionEngine::new(EngineConfig {
            mode: Mode::Auto,
            workspace: dir.path().to_path_buf(),
            read_dirs: vec![],
            rules: RuleSet::default(),
            sandbox_available: false,
            writes_need_approval: false,
        }));
        let agent = Agent::new(
            MockProvider::new(vec![]),
            ToolRegistry::new(vec![]),
            policy,
            Arc::new(NonInteractive),
            AgentConfig::new("mock/m", "m", "system", dir.path().join("out")),
            ToolContext::new(dir.path()),
        )
        .with_session(session);
        let probe = Arc::new(LockProbe {
            path,
            free: Mutex::new(None),
        });
        let shared: Arc<dyn CommandSandbox> = probe.clone();
        end_run(agent, SessionEnd::new(Some(shared)));
        assert_eq!(*probe.free.lock().unwrap(), Some(true));
    }

    #[test]
    fn exit_codes_match_the_spec() {
        assert_eq!(exit_code(TurnEndReason::Completed, false), 0);
```

with:

```rust
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_the_spec() {
        assert_eq!(exit_code(TurnEndReason::Completed, false), 0);
```

- [ ] **Step 8: `harness` on its own**

In `crates/harness-cli/Cargo.toml`:

Replace:

```toml

[dependencies]
clap.workspace = true
harness-config.workspace = true
harness-context.workspace = true
harness-core.workspace = true
harness-providers.workspace = true
harness-sandbox.workspace = true
harness-tools.workspace = true
serde_json.workspace = true
tokio.workspace = true
tokio-util.workspace = true
```

with:

```toml

[dependencies]
clap.workspace = true
crossterm.workspace = true
harness-config.workspace = true
harness-context.workspace = true
harness-core.workspace = true
harness-providers.workspace = true
harness-sandbox.workspace = true
harness-tools.workspace = true
harness-tui.workspace = true
ratatui.workspace = true
serde_json.workspace = true
tokio.workspace = true
tokio-util.workspace = true
```

Create `crates/harness-cli/src/interactive.rs`:

```rust
//! `harness` without a subcommand: the interactive session in the terminal.

use std::{io::IsTerminal, sync::Arc};

use crossterm::event::EventStream;
use harness_config::config;
use harness_context::{commands::Commands, project::project_root};
use harness_core::{agent::NonInteractive, engine::PermissionEngine, permission::Mode};
use harness_providers::registry;
use harness_tui::{
    app::{Host, Options, Prepared},
    inline::InlineTerminal,
    style::Theme,
    terminal::{CrosstermRawMode, Modes},
    ui::Ui,
};
use ratatui::backend::CrosstermBackend;

use crate::{
    context::home,
    sessions, setup,
    setup::Setup,
    slash::{self, Message},
    start::{self, Request, Started},
    term::terminal_safe,
};

/// Why the interactive session cannot start, when stdin or stdout is not a terminal.
pub fn needs_terminal(stdin: bool, stdout: bool) -> Option<&'static str> {
    (!stdin || !stdout).then_some(
        "interactive mode needs a terminal; use `harness ask \"<prompt>\"` to run a prompt without one",
    )
}

/// Expands the project's custom commands and `/init` for the session.
struct CliHost {
    setup: Arc<Setup>,
    commands: Commands,
    policy: Arc<PermissionEngine>,
}

impl Host for CliHost {
    fn is_command(&self, name: &str) -> bool {
        self.commands.get(name).is_some()
    }

    fn prepare(&mut self, typed: &str) -> Prepared {
        let expanded =
            slash::turn_input(typed, "", Some(&self.commands), &self.setup, &*self.policy);
        let mut prepared = Prepared {
            input: expanded.input,
            notes: Vec::new(),
            warnings: Vec::new(),
        };
        for message in expanded.messages {
            match message {
                Message::Warning(text) => prepared.warnings.push(text),
                Message::Note(text) => prepared.notes.push(text),
            }
        }
        prepared
    }
}

/// Runs the interactive session; the exit code.
pub async fn run(
    model_flag: Option<String>,
    mode_flag: Option<Mode>,
    choice: sessions::Choice,
) -> u8 {
    if let Some(message) = needs_terminal(
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    ) {
        eprintln!("error: {message}");
        return 2;
    }
    let setup = match setup::load() {
        Ok(setup) => Arc::new(setup),
        Err(message) => {
            eprintln!("error: {}", terminal_safe(&message));
            return 2;
        }
    };
    let session = match sessions::open(&setup, &choice) {
        Ok(session) => session,
        Err(message) => {
            eprintln!("error: {}", terminal_safe(&message));
            return 2;
        }
    };
    let Some(model_id) = model_flag.or_else(|| setup.config.model.clone()) else {
        eprintln!(
            "error: no model configured; pass one with --model, or set `model = \"<provider>/<model>\"` in {}; `harness models` lists the models harness finds",
            setup.paths.global_config_file().display()
        );
        return 2;
    };
    let resolved = match registry::resolve(&model_id, &setup.config.providers, setup::env) {
        Ok(resolved) => resolved,
        Err(e) => {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
            return 2;
        }
    };
    let model = resolved.id.clone();
    let mode = mode_flag
        .or(setup.config.mode)
        .unwrap_or_else(|| config::default_mode(&setup.workspace));
    let commands = harness_context::commands::discover(
        &project_root(&setup.workspace),
        &setup.paths.config_dir,
        home().as_deref(),
    );
    for warning in &commands.warnings {
        eprintln!("warning: {}", terminal_safe(warning));
    }
    let Started {
        agent,
        sandbox_session,
        policy,
    } = start::start(Request {
        setup: &setup,
        mode,
        model: resolved,
        session,
        approver: Arc::new(NonInteractive),
        interactive: true,
    })
    .await;
    let history = agent.rewind_points().into_iter().map(|p| p.text).collect();
    let options = Options {
        theme: Theme::from_env(),
        model,
        mode,
        commands: commands.listing(),
        workspace: setup.workspace.clone(),
        history,
    };
    let host = CliHost {
        setup: setup.clone(),
        commands,
        policy,
    };
    let result = terminal_session(agent, Box::new(host), options).await;
    sandbox_session.end();
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
            1
        }
    }
}

/// Runs the session on this process's terminal, and gives the terminal back as it was.
async fn terminal_session(
    agent: harness_core::agent::Agent,
    host: Box<dyn Host>,
    options: Options,
) -> std::io::Result<()> {
    // Asked before any events are read: both queries read the terminal's answer from stdin.
    let keyboard = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    let (column, row) = crossterm::cursor::position().unwrap_or((0, 0));
    let top = if column == 0 { row } else { row + 1 };
    let _modes = Modes::enter(std::io::stdout(), CrosstermRawMode, keyboard)?;
    let term = InlineTerminal::new(CrosstermBackend::new(std::io::stdout()), top)?;
    let ui = Ui::start(agent, host, term, options);
    ui.run(EventStream::new()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_terminal_it_names_harness_ask() {
        assert!(needs_terminal(true, true).is_none());
        for (stdin, stdout) in [(false, true), (true, false), (false, false)] {
            let message = needs_terminal(stdin, stdout).unwrap();
            assert!(message.contains("harness ask"), "{message}");
        }
    }
}
```

In `crates/harness-cli/src/main.rs`:

Replace (1 of 4):

```rust
mod ask;
mod context;
mod doctor;
mod models;
mod prompt;
mod sandbox;
mod sessions;
mod setup;
mod slash;
mod term;
mod trust;

```

with:

```rust
mod ask;
mod context;
mod doctor;
mod interactive;
mod models;
mod prompt;
mod sandbox;
mod sessions;
mod setup;
mod slash;
mod start;
mod term;
mod trust;

```

Replace (2 of 4):

```rust
        }
        Err(e) => e.exit(),
    };
    // Only `ask` continues a session: with another subcommand the flag would do nothing.
    if let Some(command) = cli
        .command
        .as_ref()
```

with:

```rust
        }
        Err(e) => e.exit(),
    };
    // Only `ask` and the interactive session continue a session: with another subcommand the flag
    // would do nothing.
    if let Some(command) = cli
        .command
        .as_ref()
```

Replace (3 of 4):

```rust
        };
        if let Some(flag) = flag {
            eprintln!(
                "error: {flag} continues a session, which only `harness ask` does; run `{}` without it",
                command_line(command)
            );
            return ExitCode::from(2);
```

with:

```rust
        };
        if let Some(flag) = flag {
            eprintln!(
                "error: {flag} continues a session, which only `harness ask` and `harness` alone do; run `{}` without it",
                command_line(command)
            );
            return ExitCode::from(2);
```

Replace (4 of 4):

```rust
            Some(Command::Sandbox {
                command: SandboxCommand::Doctor,
            }) => doctor::run(),
            None => {
                eprintln!("Interactive mode is not available yet; use `harness ask \"...\"`.");
                2
            }
        }
    });
    ExitCode::from(code)
```

with:

```rust
            Some(Command::Sandbox {
                command: SandboxCommand::Doctor,
            }) => doctor::run(),
            None => interactive::run(cli.model, cli.mode, session).await,
        }
    });
    ExitCode::from(code)
```

The refusal of `-c` with another subcommand now names the interactive session too. In `crates/harness-cli/tests/cli_smoke.rs`:

Replace (1 of 2):

```rust
    }
}

// Only `ask` continues a session: with another subcommand `-c` and `--resume <id>` were ignored
// silently.
#[test]
fn session_flags_with_another_subcommand_are_refused() {
    let home = tempfile::tempdir().unwrap();
```

with:

```rust
    }
}

// Only `ask` and the interactive session continue a session: with another subcommand `-c` and
// `--resume <id>` were ignored silently.
#[test]
fn session_flags_with_another_subcommand_are_refused() {
    let home = tempfile::tempdir().unwrap();
```

Replace (2 of 2):

```rust
            .assert()
            .code(2)
            .stderr(contains(format!(
                "{flag} continues a session, which only `harness ask` does; run `{command}` without it"
            )));
    }
}
```

with:

```rust
            .assert()
            .code(2)
            .stderr(contains(format!(
                "{flag} continues a session, which only `harness ask` and `harness` alone do; run `{command}` without it"
            )));
    }
}
```

- [ ] **Step 9: Run the tests, lint, and check the dependencies**

Run: `cargo test -p harness-tui -p harness-cli`
Expected: PASS. `ask_e2e` is unchanged and passes, `no_subcommand_explains_that_interactive_mode_is_not_ready` included (its stdin is not a terminal).

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo deny check`
Expected: clean.

- [ ] **Step 10: Commit**

```bash
git add Cargo.toml Cargo.lock crates/harness-tui crates/harness-cli
git commit -F - <<'EOF'
feat(cli): run harness interactively in the terminal

`harness` without a subcommand (and `harness -c` or `--resume <id>`)
starts the interactive session: the agent runs in a task of its own
while the inline UI takes keys and pastes, streams the reply, and
writes finished messages into the scrollback. Esc interrupts a turn,
Ctrl+C twice exits, /help lists commands and keys, and custom commands
and /init expand as in `harness ask`. Both frontends now start their
agent through start.rs; without a terminal, harness names `harness
ask` and exits 2 as before.

<trailer lines from the controller>
EOF
```

---

### Task 7: Status line, turn stats, `/context` and `/usage`

**Files:**
- Modify: `crates/harness-core/src/{event,agent}.rs`, `crates/harness-tui/src/{app,transcript,ui,lib}.rs`, `crates/harness-cli/src/{context,interactive}.rs`
- Create: `crates/harness-tui/src/status.rs`
- Test: `crates/harness-core/tests/stats.rs`, `crates/harness-tui/tests/status.rs`, `crates/harness-tui/tests/session.rs`

**Interfaces:**
- Consumes: Task 6's `App`, `Ui`, `Options`.
- Produces:
  - `AgentEvent::TurnStats { model, time_to_first_token_ms: Option<u64>, generation_ms, input_tokens, output_tokens, cached_tokens }`, sent just before `TurnFinished` in every turn that called the model;
  - `agent::ContextUsage { window, system, tools, messages, total }` and `Agent::context_usage() -> ContextUsage`;
  - `status::{tokens, thousands, status_line, stats_line, context_report, Totals}`;
  - `Options { …, instruction_files: Vec<(String, u64)>, window_note: Option<String> }`, `App::set_context(ContextUsage)`; the runner sends the context after each job;
  - `context::instruction_files(&Setup) -> Vec<(String, u64)>` in `harness-cli`.

- [ ] **Step 1: Write the failing tests**

Create `crates/harness-core/tests/stats.rs`:

```rust
//! Per-turn statistics and where the context goes.

mod common;

use std::{sync::Arc, time::Duration};

use common::{Echo, agent, run};
use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    engine::{EngineConfig, PermissionEngine},
    event::{AgentEvent, TurnEndReason},
    message::{ChatRequest, Usage},
    permission::Mode,
    provider::{FinishReason, Provider, ProviderEvent, ProviderStream},
    testing::{MockProvider, Script},
    tokens::DEFAULT_CONTEXT_WINDOW,
    tool::{ToolContext, ToolRegistry},
};
use serde_json::json;

/// A provider whose reply starts 400 ms after the request and streams for a second.
struct Slow;

impl Provider for Slow {
    fn stream(&self, _request: ChatRequest) -> ProviderStream {
        Box::pin(async_stream())
    }
}

fn async_stream()
-> impl futures::Stream<Item = Result<ProviderEvent, harness_core::provider::ProviderError>> + Send
{
    futures::stream::unfold(0, |step| async move {
        let event = match step {
            0 => {
                tokio::time::sleep(Duration::from_millis(400)).await;
                ProviderEvent::TextDelta("hel".into())
            }
            1 => {
                tokio::time::sleep(Duration::from_millis(1000)).await;
                ProviderEvent::TextDelta("lo".into())
            }
            2 => ProviderEvent::Usage(Usage {
                input_tokens: 1_000,
                output_tokens: 50,
                cached_tokens: 800,
            }),
            3 => ProviderEvent::Finished(FinishReason::Stop),
            _ => return None,
        };
        Some((Ok(event), step + 1))
    })
}

fn stats(events: &[AgentEvent]) -> Vec<AgentEvent> {
    events
        .iter()
        .filter(|e| matches!(e, AgentEvent::TurnStats { .. }))
        .cloned()
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_turn_reports_its_stats_just_before_it_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode: Mode::Auto,
        workspace: dir.path().to_path_buf(),
        read_dirs: vec![],
        rules: Default::default(),
        sandbox_available: false,
        writes_need_approval: false,
    }));
    let mut agent = Agent::new(
        Arc::new(Slow),
        ToolRegistry::new(vec![Arc::new(Echo)]),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m1", "m1", "system prompt", dir.path().join("out")),
        ToolContext::new(dir.path()),
    );
    let (reason, events) = run(&mut agent, "hi").await;
    assert_eq!(reason, TurnEndReason::Completed);
    let n = events.len();
    assert_eq!(
        events[n - 2],
        AgentEvent::TurnStats {
            model: "mock/m1".into(),
            time_to_first_token_ms: Some(400),
            generation_ms: 1000,
            input_tokens: 1_000,
            output_tokens: 50,
            cached_tokens: 800,
        }
    );
    assert!(matches!(events[n - 1], AgentEvent::TurnFinished { .. }));
}

#[tokio::test]
async fn stats_add_up_the_turns_model_calls() {
    let dir = tempfile::tempdir().unwrap();
    let usage = |input, output| {
        Ok(ProviderEvent::Usage(Usage {
            input_tokens: input,
            output_tokens: output,
            cached_tokens: 0,
        }))
    };
    let provider = MockProvider::new(vec![
        Script::Reply(vec![
            Ok(ProviderEvent::ToolCall(harness_core::message::ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                arguments: json!({"text": "x"}).to_string(),
            })),
            usage(100, 10),
            Ok(ProviderEvent::Finished(FinishReason::ToolCalls)),
        ]),
        Script::Reply(vec![
            Ok(ProviderEvent::TextDelta("done".into())),
            usage(150, 5),
            Ok(ProviderEvent::Finished(FinishReason::Stop)),
        ]),
    ]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let (_, events) = run(&mut agent, "go").await;
    let [
        AgentEvent::TurnStats {
            model,
            input_tokens,
            output_tokens,
            cached_tokens,
            time_to_first_token_ms,
            ..
        },
    ] = &stats(&events)[..]
    else {
        panic!("one TurnStats: {events:#?}");
    };
    assert_eq!(model, "mock/m1");
    assert_eq!(
        (*input_tokens, *output_tokens, *cached_tokens),
        (250, 15, 0)
    );
    assert!(time_to_first_token_ms.is_some());
}

#[tokio::test]
async fn a_turn_that_never_called_the_model_has_no_stats() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(
        MockProvider::new(vec![Script::text("never asked")]),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    let (_, events) = common::run_with(&mut agent, "stop", cancel).await;
    assert!(stats(&events).is_empty(), "{events:#?}");
}

#[tokio::test]
async fn context_usage_splits_the_next_request() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::Reply(vec![
        Ok(ProviderEvent::TextDelta("hello".into())),
        Ok(ProviderEvent::Usage(Usage {
            input_tokens: 700,
            output_tokens: 2,
            cached_tokens: 0,
        })),
        Ok(ProviderEvent::Finished(FinishReason::Stop)),
    ])]);
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir.path());
    let before = agent.context_usage();
    assert_eq!(before.window, DEFAULT_CONTEXT_WINDOW);
    assert_eq!(before.system, 4); // "system prompt" is 13 bytes
    assert!(before.tools > 100, "{before:?}");
    assert_eq!(before.messages, 0);
    assert_eq!(before.total, before.system + before.tools);
    run(&mut agent, "x".repeat(400).as_str()).await;
    let after = agent.context_usage();
    assert!(after.messages >= 100, "{after:?}");
    // The provider reported 700 input tokens for the request; the reply is estimated.
    assert_eq!(after.total, 700 + 2 + 4);
}
```

Create `crates/harness-tui/tests/status.rs`:

```rust
//! The status line, the stats after each turn, `/context` and `/usage`.

use std::{path::Path, sync::Arc, time::Duration};

use harness_core::{
    agent::{Agent, AgentConfig, ContextUsage, NonInteractive},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    message::Usage,
    permission::Mode,
    provider::{FinishReason, ProviderEvent},
    testing::{MockProvider, Script},
    tool::{ToolContext, ToolRegistry},
};
use harness_tui::{
    app::{Host, Options, Prepared},
    inline::InlineTerminal,
    status::{self, Totals},
    style::Theme,
    text::plain,
    ui::Ui,
};
use ratatui::{
    backend::TestBackend,
    buffer::Buffer,
    crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers},
};

struct NoCommands;

impl Host for NoCommands {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
}

fn start(provider: Arc<MockProvider>, dir: &Path, system: &str) -> Ui<TestBackend> {
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode: Mode::Auto,
        workspace: dir.to_path_buf(),
        read_dirs: Vec::new(),
        rules: RuleSet::default(),
        sandbox_available: false,
        writes_need_approval: false,
    }));
    let agent = Agent::new(
        provider,
        ToolRegistry::new(Vec::new()),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m", "m", system, dir.join("out")),
        ToolContext::new(dir),
    );
    let options = Options {
        theme: Theme::monochrome(),
        model: "mock/m".into(),
        mode: Mode::Auto,
        commands: Vec::new(),
        workspace: dir.to_path_buf(),
        history: Vec::new(),
        instruction_files: vec![("AGENTS.md".into(), 2_000)],
        window_note: Some("assumed".into()),
    };
    let term = InlineTerminal::new(TestBackend::new(70, 20), 0).unwrap();
    let mut ui = Ui::start(agent, Box::new(NoCommands), term, options);
    ui.draw().unwrap();
    ui
}

fn rows(buffer: &Buffer) -> Vec<String> {
    let width = buffer.area.width as usize;
    buffer
        .content
        .chunks(width.max(1))
        .map(|row| {
            let text: String = row.iter().map(|cell| cell.symbol()).collect();
            text.trim_end().to_string()
        })
        .collect()
}

fn everything(ui: &Ui<TestBackend>) -> Vec<String> {
    let backend = ui.terminal().backend();
    let mut out = rows(backend.scrollback());
    out.extend(rows(backend.buffer()));
    out
}

fn row_with(ui: &Ui<TestBackend>, text: &str) -> String {
    everything(ui)
        .into_iter()
        .find(|r| r.contains(text))
        .unwrap_or_else(|| panic!("no row with {text:?}: {:#?}", everything(ui)))
}

fn send(ui: &mut Ui<TestBackend>, text: &str) {
    for c in text.chars() {
        ui.handle(Event::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )))
        .unwrap();
    }
    ui.handle(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
        .unwrap();
    ui.handle(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
    .unwrap();
}

async fn settle(ui: &mut Ui<TestBackend>) {
    tokio::time::timeout(Duration::from_secs(10), ui.settle())
        .await
        .expect("the turn ends")
        .unwrap();
}

fn reply_with_usage(text: &str, input: u64, output: u64, cached: u64) -> Script {
    Script::Reply(vec![
        Ok(ProviderEvent::TextDelta(text.into())),
        Ok(ProviderEvent::Usage(Usage {
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
        })),
        Ok(ProviderEvent::Finished(FinishReason::Stop)),
    ])
}

#[test]
fn token_counts_are_short_in_the_status_line_and_exact_in_reports() {
    assert_eq!(status::tokens(950), "950");
    assert_eq!(status::tokens(1_234), "1.2k");
    assert_eq!(status::tokens(34_567), "34k");
    assert_eq!(status::tokens(1_500_000), "1.5M");
    assert_eq!(status::thousands(24_353), "24,353");
    assert_eq!(status::thousands(999), "999");
}

#[test]
fn the_status_line_shows_the_model_mode_context_and_tokens() {
    let theme = Theme::monochrome();
    let context = ContextUsage {
        window: 32_768,
        system: 1_000,
        tools: 1_000,
        messages: 1_000,
        total: 3_277,
    };
    let mut totals = Totals::default();
    totals.add(
        "mock/m",
        &Usage {
            input_tokens: 12_000,
            output_tokens: 1_100,
            cached_tokens: 0,
        },
    );
    let line = status::status_line("mock/m", Mode::Auto, &context, &totals, &theme);
    assert_eq!(
        plain(&line),
        "mock/m · auto · 11% of context · 12k in, 1.1k out"
    );
    let full = status::status_line("mock/m", Mode::FullAccess, &context, &totals, &theme);
    assert!(plain(&full).ends_with("· full-access: no sandbox, no approvals"));
}

#[test]
fn the_stats_line_shows_what_the_provider_reported() {
    let theme = Theme::monochrome();
    let line = status::stats_line("ollama/qwen", Some(400), 1_000, 1_000, 50, 800, &theme);
    assert_eq!(
        plain(&line),
        "ollama/qwen · first token 0.4s · 50 tok/s · cache 80.0%"
    );
    // No cached tokens reported: no cache rate.
    let line = status::stats_line("openai/x", Some(1_250), 2_000, 900, 100, 0, &theme);
    assert_eq!(plain(&line), "openai/x · first token 1.2s · 50 tok/s");
}

#[tokio::test]
async fn a_turn_on_a_local_model_ends_with_its_stats_and_updates_the_status_line() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![reply_with_usage("Done.", 1_000, 50, 800)]);
    let mut ui = start(provider, dir.path(), "system");
    assert!(row_with(&ui, "mock/m · auto").contains("0 in, 0 out"));
    send(&mut ui, "go");
    settle(&mut ui).await;
    let stats = row_with(&ui, "first token");
    assert!(stats.starts_with("mock/m · first token"), "{stats}");
    assert!(stats.ends_with("· cache 80.0%"), "{stats}");
    let status = row_with(&ui, "mock/m · auto");
    assert!(status.contains("1.0k in, 50 out"), "{status}");
    // The provider reported 1,000 input tokens: about 3% of the 32,768-token window.
    assert!(status.contains(" · 4% of context"), "{status}");
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn context_lists_each_instruction_file_and_the_free_space() {
    let dir = tempfile::tempdir().unwrap();
    // A system prompt holding a 2,000-token AGENTS.md.
    let system = format!("base prompt\n{}", "a".repeat(8_000));
    let mut ui = start(MockProvider::new(Vec::new()), dir.path(), &system);
    send(&mut ui, "/context");
    assert!(row_with(&ui, "Context window").contains("32,768 tokens (assumed)"));
    let agents = row_with(&ui, "AGENTS.md");
    assert!(agents.contains("2,000"), "{agents}");
    assert!(agents.contains("6.1%"), "{agents}");
    let prompt = row_with(&ui, "system prompt");
    assert!(prompt.contains(" 3 "), "{prompt}");
    assert!(row_with(&ui, "tool definitions").contains(" 1 "));
    let free = row_with(&ui, "free");
    assert!(free.contains("30,764"), "{free}");
    assert!(everything(&ui).iter().any(|r| r.contains("conversation")));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn usage_shows_tokens_per_model() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        reply_with_usage("one", 1_000, 50, 800),
        reply_with_usage("two", 1_200, 70, 1_000),
    ]);
    let mut ui = start(provider, dir.path(), "system");
    send(&mut ui, "/usage");
    assert!(row_with(&ui, "No tokens used yet").contains("session"));
    send(&mut ui, "first");
    settle(&mut ui).await;
    send(&mut ui, "second");
    settle(&mut ui).await;
    send(&mut ui, "/usage");
    let header = row_with(&ui, "model");
    assert!(
        header.contains("input") && header.contains("cached"),
        "{header}"
    );
    let row = row_with(&ui, "2,200");
    let cells: Vec<&str> = row.split_whitespace().collect();
    assert_eq!(cells, ["mock/m", "2,200", "120", "1,800"]);
    ui.finish().await.unwrap();
}
```

In `crates/harness-tui/tests/session.rs`:

Replace:

```rust
        ],
        workspace: dir.to_path_buf(),
        history: Vec::new(),
    }
}

```

with:

```rust
        ],
        workspace: dir.to_path_buf(),
        history: Vec::new(),
        instruction_files: Vec::new(),
        window_note: None,
    }
}

```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p harness-core --test stats`
Expected: FAIL to compile: ``no variant named `TurnStats` found for enum `AgentEvent` `` and ``no method named `context_usage` found for struct `Agent` ``.

Run: `cargo test -p harness-tui --test status --test session`
Expected: FAIL to compile: ``unresolved import `harness_core::agent::ContextUsage` ``, ``unresolved import `harness_tui::status` `` and ``struct `harness_tui::app::Options` has no field named `instruction_files` ``.

- [ ] **Step 3: Stats and context usage in the core**

In `crates/harness-core/src/event.rs`:

Replace:

```rust
        tokens_before: u64,
        tokens_after: u64,
    },
    TurnFinished {
        reason: TurnEndReason,
    },
```

with:

```rust
        tokens_before: u64,
        tokens_after: u64,
    },
    /// How the turn went, just before it finishes, when it called the model: the model that
    /// answered last, how long its first output took, and the tokens the provider reported.
    TurnStats {
        model: String,
        /// From the request to the first output, of the turn's first reply that had any.
        time_to_first_token_ms: Option<u64>,
        /// Time spent streaming output: from each reply's first output to its end.
        generation_ms: u64,
        input_tokens: u64,
        output_tokens: u64,
        cached_tokens: u64,
    },
    TurnFinished {
        reason: TurnEndReason,
    },
```

In `crates/harness-core/src/agent.rs`:

Replace (1 of 12):

```rust
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::{
```

with:

```rust
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::{sync::mpsc::UnboundedSender, time::Instant};
use tokio_util::sync::CancellationToken;

use crate::{
```

Replace (2 of 12):

```rust
    }
}

/// What one model call produced so far. Kept outside the stream future so partial output survives.
#[derive(Debug, Default)]
struct ModelReply {
```

with:

```rust
    }
}

/// Where the next request's tokens go, estimated, and the model's context window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextUsage {
    /// The context window, in tokens.
    pub window: u64,
    /// The system prompt, instruction files and environment included.
    pub system: u64,
    /// The tool definitions.
    pub tools: u64,
    /// The conversation.
    pub messages: u64,
    /// The whole next request: from the input tokens the provider reported for the last one
    /// where it did, else the sum of the estimates above.
    pub total: u64,
}

/// What a turn's model calls took, for its [`AgentEvent::TurnStats`].
#[derive(Debug, Default)]
struct Stats {
    /// The model that answered last; `None` until one was asked.
    model: Option<String>,
    time_to_first_token: Option<Duration>,
    generation: Duration,
    usage: Usage,
}

/// What one model call produced so far. Kept outside the stream future so partial output survives.
#[derive(Debug, Default)]
struct ModelReply {
```

Replace (3 of 12):

```rust
    emitted: bool,
    /// The token counts the provider reported for this call.
    usage: Option<Usage>,
}

/// Why the conversation is being compacted.
```

with:

```rust
    emitted: bool,
    /// The token counts the provider reported for this call.
    usage: Option<Usage>,
    /// When the request was sent, when the first output arrived, and when the stream ended.
    started: Option<Instant>,
    first_output: Option<Instant>,
    ended: Option<Instant>,
}

/// Why the conversation is being compacted.
```

Replace (4 of 12):

```rust
    /// compaction then waits until the estimate drops below it, rather than repeat at every step
    /// without shrinking anything.
    auto_compaction_paused: bool,
}

impl Agent {
```

with:

```rust
    /// compaction then waits until the estimate drops below it, rather than repeat at every step
    /// without shrinking anything.
    auto_compaction_paused: bool,
    /// The current turn's model calls, for its stats.
    stats: Stats,
}

impl Agent {
```

Replace (5 of 12):

```rust
            next_call_id: 0,
            turn_model: None,
            auto_compaction_paused: false,
        }
    }

```

with:

```rust
            next_call_id: 0,
            turn_model: None,
            auto_compaction_paused: false,
            stats: Stats::default(),
        }
    }

```

Replace (6 of 12):

```rust
        &mut self.config
    }

    /// Invalid tool calls (unknown tool, bad JSON, schema violations) in the current or last turn.
    pub fn invalid_calls_this_turn(&self) -> u32 {
        self.invalid_calls
```

with:

```rust
        &mut self.config
    }

    /// Where the next request's tokens would go.
    pub fn context_usage(&self) -> ContextUsage {
        let system = crate::tokens::estimate(&self.config.system_prompt);
        let tools = compaction::request_tokens("", &self.tools.specs(), &[]);
        let messages = self.history.iter().map(compaction::message_tokens).sum();
        ContextUsage {
            window: self.config.context_window,
            system,
            tools,
            messages,
            total: self.estimated_tokens(),
        }
    }

    /// Invalid tool calls (unknown tool, bad JSON, schema violations) in the current or last turn.
    pub fn invalid_calls_this_turn(&self) -> u32 {
        self.invalid_calls
```

Replace (7 of 12):

```rust
        let input = input.into();
        self.ctx.cancel = cancel.clone();
        self.invalid_calls = 0;
        self.turn_checkpointed = false;
        // Settings that apply to this turn only.
        self.turn_model = input.model.clone();
```

with:

```rust
        let input = input.into();
        self.ctx.cancel = cancel.clone();
        self.invalid_calls = 0;
        self.stats = Stats::default();
        self.turn_checkpointed = false;
        // Settings that apply to this turn only.
        self.turn_model = input.model.clone();
```

Replace (8 of 12):

```rust
            if !auto_compaction_failed {
                auto_compaction_failed = !self.compact_automatically(events, &cancel).await;
            }
            let reply = match self.call_model_compacting(events, &cancel).await {
                ModelOutcome::Reply(mut reply) => {
                    self.dedupe_call_ids(&mut reply.tool_calls);
                    // The reported input covers the request; the reply is estimated like any
```

with:

```rust
            if !auto_compaction_failed {
                auto_compaction_failed = !self.compact_automatically(events, &cancel).await;
            }
            let outcome = self.call_model_compacting(events, &cancel).await;
            match &outcome {
                ModelOutcome::Reply(reply)
                | ModelOutcome::Failed(_, reply)
                | ModelOutcome::Interrupted(reply) => self.tally(reply),
            }
            let reply = match outcome {
                ModelOutcome::Reply(mut reply) => {
                    self.dedupe_call_ids(&mut reply.tool_calls);
                    // The reported input covers the request; the reply is estimated like any
```

Replace (9 of 12):

```rust
        self.record(message, None, false);
    }

    fn finish(
        &mut self,
        reason: TurnEndReason,
```

with:

```rust
        self.record(message, None, false);
    }

    /// Adds what a model call took to the turn's stats.
    fn tally(&mut self, reply: &ModelReply) {
        self.stats.model = Some(self.model_id().to_string());
        if let (Some(started), Some(first)) = (reply.started, reply.first_output) {
            if self.stats.time_to_first_token.is_none() {
                self.stats.time_to_first_token = Some(first - started);
            }
            let ended = reply.ended.unwrap_or_else(Instant::now);
            self.stats.generation += ended.saturating_duration_since(first);
        }
        if let Some(usage) = reply.usage {
            self.stats.usage.input_tokens += usage.input_tokens;
            self.stats.usage.output_tokens += usage.output_tokens;
            self.stats.usage.cached_tokens += usage.cached_tokens;
        }
    }

    fn finish(
        &mut self,
        reason: TurnEndReason,
```

Replace (10 of 12):

```rust
    ) -> TurnEndReason {
        for message in self.warnings.drain(..) {
            let _ = events.send(AgentEvent::Warning { message });
        }
        let _ = events.send(AgentEvent::TurnFinished { reason });
        reason
```

with:

```rust
    ) -> TurnEndReason {
        for message in self.warnings.drain(..) {
            let _ = events.send(AgentEvent::Warning { message });
        }
        let stats = std::mem::take(&mut self.stats);
        if let Some(model) = stats.model {
            let _ = events.send(AgentEvent::TurnStats {
                model,
                time_to_first_token_ms: stats.time_to_first_token.map(|d| d.as_millis() as u64),
                generation_ms: stats.generation.as_millis() as u64,
                input_tokens: stats.usage.input_tokens,
                output_tokens: stats.usage.output_tokens,
                cached_tokens: stats.usage.cached_tokens,
            });
        }
        let _ = events.send(AgentEvent::TurnFinished { reason });
        reason
```

Replace (11 of 12):

```rust
            messages: request_messages(&self.history),
            tools: self.tools.specs(),
        };
        let mut stream = provider.stream(request);
        while let Some(item) = stream.next().await {
            match item? {
                ProviderEvent::TextDelta(text) => {
                    reply.emitted = true;
                    reply.text.push_str(&text);
```

with:

```rust
            messages: request_messages(&self.history),
            tools: self.tools.specs(),
        };
        reply.started = Some(Instant::now());
        let mut stream = provider.stream(request);
        while let Some(item) = stream.next().await {
            let item = item?;
            if matches!(
                item,
                ProviderEvent::TextDelta(_)
                    | ProviderEvent::ReasoningDelta(_)
                    | ProviderEvent::ToolCall(_)
            ) && reply.first_output.is_none()
            {
                reply.first_output = Some(Instant::now());
            }
            match item {
                ProviderEvent::TextDelta(text) => {
                    reply.emitted = true;
                    reply.text.push_str(&text);
```

Replace (12 of 12):

```rust
                ProviderEvent::Finished(reason) => reply.finish = Some(reason),
            }
        }
        Ok(())
    }

```

with:

```rust
                ProviderEvent::Finished(reason) => reply.finish = Some(reason),
            }
        }
        reply.ended = Some(Instant::now());
        Ok(())
    }

```

Run: `cargo test -p harness-core`
Expected: PASS, including `a_turn_reports_its_stats_just_before_it_finishes`.

- [ ] **Step 4: The status line and the reports**

Create `crates/harness-tui/src/status.rs`:

```rust
//! The numbers the session shows: the status line, the stats after each turn, `/context` and
//! `/usage`.

use std::collections::BTreeMap;

use harness_core::{agent::ContextUsage, message::Usage, permission::Mode};
use ratatui::text::{Line, Span};

use crate::{style::Theme, text::sanitize};

/// `n` tokens, short: `950`, `1.2k`, `34k`, `1.5M`.
pub fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..10_000 => format!("{:.1}k", n as f64 / 1_000.0),
        10_000..1_000_000 => format!("{}k", n / 1_000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// `n` with thousands separators: `24,353`.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `part` as a percentage of `whole`, one decimal.
fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "0%".into();
    }
    format!("{:.1}%", part as f64 * 100.0 / whole as f64)
}

/// The session's token counts from the provider, per model.
#[derive(Debug, Clone, Default)]
pub struct Totals {
    per_model: BTreeMap<String, Usage>,
}

impl Totals {
    pub fn add(&mut self, model: &str, usage: &Usage) {
        let total = self.per_model.entry(model.to_string()).or_default();
        total.input_tokens += usage.input_tokens;
        total.output_tokens += usage.output_tokens;
        total.cached_tokens += usage.cached_tokens;
    }

    /// Input and output tokens over every model.
    pub fn sum(&self) -> (u64, u64) {
        self.per_model.values().fold((0, 0), |(i, o), u| {
            (i + u.input_tokens, o + u.output_tokens)
        })
    }

    /// `/usage`: input, output and cached tokens per model.
    pub fn report(&self, theme: &Theme) -> Vec<Line<'static>> {
        if self.per_model.is_empty() {
            return vec![Line::from(Span::styled(
                "No tokens used yet in this session.",
                theme.dim(),
            ))];
        }
        let width = self
            .per_model
            .keys()
            .map(|m| m.chars().count())
            .max()
            .unwrap_or(0)
            .max(5);
        let mut lines = vec![Line::from(Span::styled(
            format!(
                "{:width$}  {:>10}  {:>10}  {:>10}",
                "model", "input", "output", "cached"
            ),
            theme.bold(),
        ))];
        for (model, usage) in &self.per_model {
            lines.push(Line::from(format!(
                "{:width$}  {:>10}  {:>10}  {:>10}",
                sanitize(model),
                thousands(usage.input_tokens),
                thousands(usage.output_tokens),
                thousands(usage.cached_tokens),
            )));
        }
        lines.push(Line::from(Span::styled(
            "Counted since harness started, as each provider reported them.",
            theme.dim(),
        )));
        lines
    }
}

/// The status line: the model, the approval mode, how full the context window is, and the
/// session's tokens.
pub fn status_line(
    model: &str,
    mode: Mode,
    context: &ContextUsage,
    totals: &Totals,
    theme: &Theme,
) -> Line<'static> {
    let (input, output) = totals.sum();
    let used = if context.window == 0 {
        0
    } else {
        (context.total * 100).div_ceil(context.window)
    };
    let mut spans = vec![Span::styled(
        format!(
            "{} · {mode} · {used}% of context · {} in, {} out",
            sanitize(model),
            tokens(input),
            tokens(output)
        ),
        theme.dim(),
    )];
    if mode == Mode::FullAccess {
        spans.push(Span::styled(
            " · full-access: no sandbox, no approvals",
            theme.error(),
        ));
    }
    Line::from(spans)
}

/// The dim line after a turn: the model that answered, time to first token, output tokens per
/// second, and the prompt-cache hit rate when the provider reported cached tokens.
pub fn stats_line(
    model: &str,
    time_to_first_token_ms: Option<u64>,
    generation_ms: u64,
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
    theme: &Theme,
) -> Line<'static> {
    let mut parts = vec![sanitize(model)];
    if let Some(ms) = time_to_first_token_ms {
        parts.push(format!("first token {:.1}s", ms as f64 / 1_000.0));
    }
    if output_tokens > 0 && generation_ms > 0 {
        let rate = output_tokens as f64 * 1_000.0 / generation_ms as f64;
        parts.push(format!("{rate:.0} tok/s"));
    }
    if cached_tokens > 0 && input_tokens > 0 {
        parts.push(format!("cache {}", percent(cached_tokens, input_tokens)));
    }
    Line::from(Span::styled(parts.join(" · "), theme.dim()))
}

/// `/context`: the context window, and how the next request fills it: the system prompt, the
/// tool definitions, each instruction file (its tokens are part of the system prompt), the
/// conversation, and what is free.
pub fn context_report(
    context: &ContextUsage,
    instruction_files: &[(String, u64)],
    window_note: Option<&str>,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let files: u64 = instruction_files.iter().map(|(_, t)| *t).sum();
    let mut rows: Vec<(String, u64)> = vec![
        ("system prompt".into(), context.system.saturating_sub(files)),
        ("tool definitions".into(), context.tools),
    ];
    rows.extend(
        instruction_files
            .iter()
            .map(|(name, tokens)| (sanitize(name), *tokens)),
    );
    rows.push(("conversation".into(), context.messages));
    let used = context.system + context.tools + context.messages;
    rows.push(("free".into(), context.window.saturating_sub(used)));
    let mut title = format!("Context window: {} tokens", thousands(context.window));
    if let Some(note) = window_note {
        title.push_str(&format!(" ({})", sanitize(note)));
    }
    let name_width = rows
        .iter()
        .map(|(n, _)| n.chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = vec![Line::from(Span::styled(title, theme.bold()))];
    for (name, tokens) in rows {
        lines.push(Line::from(vec![
            Span::raw(format!("  {name:name_width$}  ")),
            Span::raw(format!("{:>9}", thousands(tokens))),
            Span::styled(
                format!("  {:>6}", percent(tokens, context.window)),
                theme.dim(),
            ),
        ]));
    }
    lines
}
```

In `crates/harness-tui/src/transcript.rs`:

Replace:

```rust
                    TurnEndReason::Completed | TurnEndReason::Error => {}
                }
            }
            AgentEvent::ApprovalNeeded { .. }
            | AgentEvent::Usage { .. }
            | AgentEvent::CheckpointCreated { .. } => {}
```

with:

```rust
                    TurnEndReason::Completed | TurnEndReason::Error => {}
                }
            }
            AgentEvent::TurnStats {
                model,
                time_to_first_token_ms,
                generation_ms,
                input_tokens,
                output_tokens,
                cached_tokens,
            } => {
                let line = crate::status::stats_line(
                    model,
                    *time_to_first_token_ms,
                    *generation_ms,
                    *input_tokens,
                    *output_tokens,
                    *cached_tokens,
                    &self.theme,
                );
                self.gap();
                self.push_lines(vec![line], width);
            }
            AgentEvent::ApprovalNeeded { .. }
            | AgentEvent::Usage { .. }
            | AgentEvent::CheckpointCreated { .. } => {}
```

In `crates/harness-tui/src/app.rs`:

Replace (1 of 8):

```rust
use std::time::{Duration, Instant};

use harness_context::commands::{is_builtin, parse_invocation};
use harness_core::{event::AgentEvent, permission::Mode, turn::TurnInput};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::Position,
```

with:

```rust
use std::time::{Duration, Instant};

use harness_context::commands::{is_builtin, parse_invocation};
use harness_core::{agent::ContextUsage, event::AgentEvent, permission::Mode, turn::TurnInput};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::Position,
```

Replace (2 of 8):

```rust
use crate::{
    complete::{self, Completer, Offer},
    editor::{Edit, Editor},
    style::Theme,
    text::{sanitize, wrap},
    transcript::Transcript,
```

with:

```rust
use crate::{
    complete::{self, Completer, Offer},
    editor::{Edit, Editor},
    status::{self, Totals},
    style::Theme,
    text::{sanitize, wrap},
    transcript::Transcript,
```

Replace (3 of 8):

```rust
    pub workspace: std::path::PathBuf,
    /// Earlier inputs, oldest first, for Up.
    pub history: Vec<String>,
}

/// What the session should do after a key.
```

with:

```rust
    pub workspace: std::path::PathBuf,
    /// Earlier inputs, oldest first, for Up.
    pub history: Vec<String>,
    /// The instruction files in the system prompt, with their estimated tokens, for `/context`.
    pub instruction_files: Vec<(String, u64)>,
    /// Said next to the context window in `/context`, such as where its size comes from.
    pub window_note: Option<String>,
}

/// What the session should do after a key.
```

Replace (4 of 8):

```rust
    ctrl_c: Option<Instant>,
    /// From the moment a turn is asked for until it has finished.
    running: bool,
    width: usize,
}

```

with:

```rust
    ctrl_c: Option<Instant>,
    /// From the moment a turn is asked for until it has finished.
    running: bool,
    /// Where the next request's tokens go, as of the end of the last turn.
    context: ContextUsage,
    totals: Totals,
    instruction_files: Vec<(String, u64)>,
    window_note: Option<String>,
    width: usize,
}

```

Replace (5 of 8):

```rust
            hint: None,
            ctrl_c: None,
            running: false,
            width,
        }
    }
```

with:

```rust
            hint: None,
            ctrl_c: None,
            running: false,
            context: ContextUsage::default(),
            totals: Totals::default(),
            instruction_files: options.instruction_files,
            window_note: options.window_note,
            width,
        }
    }
```

Replace (6 of 8):

```rust

    /// Takes in an event from the agent.
    pub fn on_event(&mut self, event: &AgentEvent) {
        if let AgentEvent::TurnFinished { .. } = event {
            self.running = false;
        }
        self.transcript.on_event(event, self.width);
    }

    /// Asks for a turn.
```

with:

```rust

    /// Takes in an event from the agent.
    pub fn on_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::TurnFinished { .. } => self.running = false,
            AgentEvent::Usage { model, usage } => self.totals.add(model, usage),
            _ => {}
        }
        self.transcript.on_event(event, self.width);
    }

    /// Where the next request's tokens go, now.
    pub fn set_context(&mut self, context: ContextUsage) {
        self.context = context;
    }

    /// Asks for a turn.
```

Replace (7 of 8):

```rust
            if name == "quit" {
                return Some(Action::Quit);
            }
            if let Some((_, when)) = LATER.iter().find(|(n, _)| *n == name) {
                self.editor.submit();
                self.transcript.push_user(&full, width);
```

with:

```rust
            if name == "quit" {
                return Some(Action::Quit);
            }
            if name == "context" || name == "usage" {
                self.editor.submit();
                self.transcript.push_user(&full, width);
                let theme = self.theme();
                let lines = if name == "context" {
                    status::context_report(
                        &self.context,
                        &self.instruction_files,
                        self.window_note.as_deref(),
                        &theme,
                    )
                } else {
                    self.totals.report(&theme)
                };
                self.transcript.push_lines(lines, width);
                return None;
            }
            if let Some((_, when)) = LATER.iter().find(|(n, _)| *n == name) {
                self.editor.submit();
                self.transcript.push_user(&full, width);
```

Replace (8 of 8):

```rust

    /// The status line.
    fn status(&self) -> Line<'static> {
        let theme = self.theme();
        Line::from(Span::styled(
            format!("{} · {}", sanitize(&self.model), self.mode),
            theme.dim(),
        ))
    }

    /// The live region's lines, at most `rows`, and where the cursor is in them.
```

with:

```rust

    /// The status line.
    fn status(&self) -> Line<'static> {
        status::status_line(
            &self.model,
            self.mode,
            &self.context,
            &self.totals,
            &self.theme(),
        )
    }

    /// The live region's lines, at most `rows`, and where the cursor is in them.
```

In `crates/harness-tui/src/ui.rs`:

Replace (1 of 7):

```rust
use std::{io, time::Instant};

use futures::{Stream, StreamExt};
use harness_core::{agent::Agent, event::AgentEvent, turn::TurnInput};
use ratatui::{backend::Backend, crossterm::event::Event};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
```

with:

```rust
use std::{io, time::Instant};

use futures::{Stream, StreamExt};
use harness_core::{
    agent::{Agent, ContextUsage},
    event::AgentEvent,
    turn::TurnInput,
};
use ratatui::{backend::Backend, crossterm::event::Event};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
```

Replace (2 of 7):

```rust
    term: InlineTerminal<B>,
    jobs: Option<mpsc::UnboundedSender<Job>>,
    events: mpsc::UnboundedReceiver<AgentEvent>,
    runner: Option<JoinHandle<()>>,
    /// Cancels the running turn.
    cancel: Option<CancellationToken>,
```

with:

```rust
    term: InlineTerminal<B>,
    jobs: Option<mpsc::UnboundedSender<Job>>,
    events: mpsc::UnboundedReceiver<AgentEvent>,
    /// Where the context goes, sent by the runner after each job.
    contexts: mpsc::UnboundedReceiver<ContextUsage>,
    /// A job was sent whose context update has not come yet.
    awaiting_context: bool,
    runner: Option<JoinHandle<()>>,
    /// Cancels the running turn.
    cancel: Option<CancellationToken>,
```

Replace (3 of 7):

```rust
    ) -> Self {
        let (jobs, mut queue) = mpsc::unbounded_channel::<Job>();
        let (events_tx, events) = mpsc::unbounded_channel();
        let runner = tokio::spawn(async move {
            let mut agent = agent;
            while let Some(job) = queue.recv().await {
```

with:

```rust
    ) -> Self {
        let (jobs, mut queue) = mpsc::unbounded_channel::<Job>();
        let (events_tx, events) = mpsc::unbounded_channel();
        let (contexts_tx, contexts) = mpsc::unbounded_channel();
        let width = term.width() as usize;
        let mut app = App::new(options, host, width);
        app.set_context(agent.context_usage());
        let runner = tokio::spawn(async move {
            let mut agent = agent;
            while let Some(job) = queue.recv().await {
```

Replace (4 of 7):

```rust
                        agent.run_turn(input, &events_tx, cancel).await;
                    }
                }
            }
        });
        let width = term.width() as usize;
        Ui {
            app: App::new(options, host, width),
            term,
            jobs: Some(jobs),
            events,
            runner: Some(runner),
            cancel: None,
        }
```

with:

```rust
                        agent.run_turn(input, &events_tx, cancel).await;
                    }
                }
                let _ = contexts_tx.send(agent.context_usage());
            }
        });
        Ui {
            app,
            term,
            jobs: Some(jobs),
            events,
            contexts,
            awaiting_context: false,
            runner: Some(runner),
            cancel: None,
        }
```

Replace (5 of 7):

```rust
                self.cancel = Some(cancel.clone());
                if let Some(jobs) = &self.jobs {
                    let _ = jobs.send(Job::Turn { input, cancel });
                }
                Flow::Continue
            }
```

with:

```rust
                self.cancel = Some(cancel.clone());
                if let Some(jobs) = &self.jobs {
                    let _ = jobs.send(Job::Turn { input, cancel });
                    self.awaiting_context = true;
                }
                Flow::Continue
            }
```

Replace (6 of 7):

```rust
        Ok(Flow::Continue)
    }

    /// Waits for the agent's next event and takes it in.
    pub async fn next(&mut self) -> io::Result<Flow> {
        match self.events.recv().await {
            Some(event) => self.agent_event(event),
            None => Ok(Flow::Quit),
        }
    }

    /// Takes in the agent's events until no turn is running.
    pub async fn settle(&mut self) -> io::Result<()> {
        while self.app.busy() || !self.events.is_empty() {
            if self.next().await? == Flow::Quit {
                break;
            }
```

with:

```rust
        Ok(Flow::Continue)
    }

    /// Takes in where the context goes after a job, and redraws the status line.
    fn context(&mut self, context: ContextUsage) -> io::Result<Flow> {
        self.awaiting_context = false;
        self.app.set_context(context);
        self.draw()?;
        Ok(Flow::Continue)
    }

    /// Waits for the agent's next event, or the runner's next message, and takes it in.
    pub async fn next(&mut self) -> io::Result<Flow> {
        tokio::select! {
            event = self.events.recv() => match event {
                Some(event) => self.agent_event(event),
                None => Ok(Flow::Quit),
            },
            Some(context) = self.contexts.recv() => self.context(context),
        }
    }

    /// Takes in the agent's events until no turn is running and the runner has said where the
    /// context goes after it.
    pub async fn settle(&mut self) -> io::Result<()> {
        while self.app.busy() || self.awaiting_context || !self.events.is_empty() {
            if self.next().await? == Flow::Quit {
                break;
            }
```

Replace (7 of 7):

```rust
                    Some(event) => self.agent_event(event)?,
                    None => Flow::Quit,
                },
            };
            if flow == Flow::Quit {
                break;
```

with:

```rust
                    Some(event) => self.agent_event(event)?,
                    None => Flow::Quit,
                },
                Some(context) = self.contexts.recv() => self.context(context)?,
            };
            if flow == Flow::Quit {
                break;
```

In `crates/harness-tui/src/lib.rs`:

Replace:

```rust
pub mod highlight;
pub mod inline;
pub mod markdown;
pub mod style;
pub mod terminal;
pub mod text;
```

with:

```rust
pub mod highlight;
pub mod inline;
pub mod markdown;
pub mod status;
pub mod style;
pub mod terminal;
pub mod text;
```

- [ ] **Step 5: The instruction files' sizes for `/context`**

Append to `crates/harness-cli/src/context.rs`, after a blank line:

```rust
/// The instruction files in the session's system prompt and their estimated tokens, for
/// `/context`: named relative to the workspace when inside it.
pub fn instruction_files(setup: &Setup) -> Vec<(String, u64)> {
    instructions::discover(&setup.workspace, &setup.paths.config_dir, home().as_deref())
        .files
        .into_iter()
        .map(|file| {
            let name = file
                .path
                .strip_prefix(&setup.workspace)
                .unwrap_or(&file.path)
                .display()
                .to_string();
            (name, harness_core::tokens::estimate(&file.content))
        })
        .collect()
}
```

In `crates/harness-cli/src/interactive.rs`:

Replace:

```rust
        commands: commands.listing(),
        workspace: setup.workspace.clone(),
        history,
    };
    let host = CliHost {
        setup: setup.clone(),
```

with:

```rust
        commands: commands.listing(),
        workspace: setup.workspace.clone(),
        history,
        instruction_files: crate::context::instruction_files(&setup),
        window_note: Some("assumed until model profiles report the model's own".into()),
    };
    let host = CliHost {
        setup: setup.clone(),
```

- [ ] **Step 6: Run the tests, and lint**

Run: `cargo test -p harness-core -p harness-tui -p harness-cli`
Expected: PASS. `ask_e2e` passes unchanged: `--json` output now has a `turn_stats` line before `turn_finished`.

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add crates/harness-core crates/harness-tui crates/harness-cli
git commit -F - <<'EOF'
feat(tui): show the status line, turn stats, /context and /usage

The agent reports a turn_stats event before each turn finishes (the
model, time to first token, streaming time and the tokens the provider
reported), and Agent::context_usage splits the next request into the
system prompt, tools and conversation. The status line shows the model,
mode, how full the context window is and the session's tokens; a dim
line after each turn shows its stats; /context and /usage report the
details. The window stays the assumed 32,768 tokens until P4.

<trailer lines from the controller>
EOF
```

---

### Task 8: Approvals at the terminal, with diffs

**Files:**
- Modify: `crates/harness-core/src/agent.rs`, `crates/harness-tui/Cargo.toml`, `crates/harness-tui/src/{app,transcript,ui,lib}.rs`, `crates/harness-cli/src/interactive.rs`
- Create: `crates/harness-tui/src/approval.rs`
- Test: `crates/harness-core/tests/approvals.rs`, `crates/harness-tui/tests/approvals.rs`, `crates/harness-tui/tests/{session,status}.rs`

**Interfaces:**
- Consumes: P2's `Approver`, `ApprovalRequest`, `ApprovalDecision`; Task 6's `App`, `Ui`; Task 2's `diff::unified`.
- Produces:
  - `agent::ApprovalKind { Action, RunUnsandboxed }` and `ApprovalRequest::kind`; a `Warning` event when a session approval cannot be kept;
  - `approval::ChannelApprover::new() -> (Arc<ChannelApprover>, Requests)`, `approval::{Requests, Reply}`, `approval::Prompt` (`new(request, reply, arguments, workspace, theme)`, `key(KeyEvent) -> Option<Answered>`, `render(width, rows, theme)`, `outcome`, `answer`, `request`), `approval::Answered { Decided(ApprovalDecision), Interrupt }`;
  - `Ui::start(agent, host, term, options, approvals: Requests)`, `Ui::until(done: impl Fn(&App) -> bool)`; `settle` stops when an approval waits;
  - `App::on_approval(request, reply)`, `App::prompt()`; `App::live` returns `Option<Position>` (no cursor while an approval waits); `Transcript::arguments(id)`.

- [ ] **Step 1: Write the failing tests**

Create `crates/harness-core/tests/approvals.rs`:

```rust
//! What approvals ask, and what an approval for the session does.

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::{agent_with_sandbox, run};
use harness_core::{
    agent::{ApprovalDecision, ApprovalKind, ApprovalRequest, Approver},
    event::AgentEvent,
    permission::Mode,
    testing::{MockProvider, Script},
};
use serde_json::json;

/// Answers with `decision`, and records each request's kind and reason.
struct Answer {
    decision: ApprovalDecision,
    asked: Mutex<Vec<(ApprovalKind, String)>>,
}

impl Answer {
    fn new(decision: ApprovalDecision) -> Arc<Answer> {
        Arc::new(Answer {
            decision,
            asked: Mutex::new(Vec::new()),
        })
    }

    fn asked(&self) -> Vec<(ApprovalKind, String)> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl Approver for Answer {
    async fn decide(&self, request: &ApprovalRequest) -> ApprovalDecision {
        self.asked
            .lock()
            .unwrap()
            .push((request.kind, request.reason.clone()));
        self.decision.clone()
    }
}

#[tokio::test]
async fn an_action_is_asked_about_as_such_and_a_rerun_outside_the_sandbox_as_such() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "touch", json!({"path": "a.txt"})),
        Script::tool_call("c2", "boxed", json!({})),
        Script::text("done"),
    ]);
    let answer = Answer::new(ApprovalDecision::Approve);
    let mut agent = agent_with_sandbox(provider, Mode::Ask, answer.clone(), dir.path());
    run(&mut agent, "go").await;
    let asked = answer.asked();
    assert_eq!(asked[0].0, ApprovalKind::Action);
    assert!(asked[0].1.contains("a.txt"), "{asked:?}");
    let rerun = asked.last().unwrap();
    assert_eq!(rerun.0, ApprovalKind::RunUnsandboxed);
    assert!(rerun.1.contains("without the sandbox"), "{asked:?}");
}

#[tokio::test]
async fn a_session_approval_that_cannot_be_kept_says_it_applied_once() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "bash", json!({"command": "git reset --hard HEAD~1"})),
        Script::tool_call("c2", "bash", json!({"command": "git reset --hard HEAD~1"})),
        Script::text("done"),
    ]);
    let answer = Answer::new(ApprovalDecision::ApproveForSession);
    let mut agent = agent_with_sandbox(provider, Mode::Auto, answer.clone(), dir.path());
    let (_, events) = run(&mut agent, "go").await;
    // A destructive command is never approved for the session: both runs asked.
    assert_eq!(answer.asked().len(), 2);
    let warnings: Vec<&String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Warning { message } => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(
        warnings[0].starts_with("approved once: ")
            && warnings[0].ends_with("so harness will ask again next time"),
        "{warnings:?}"
    );
}

#[tokio::test]
async fn a_command_approved_for_the_session_is_not_asked_about_again() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "bash", json!({"command": "cargo test"})),
        Script::tool_call("c2", "bash", json!({"command": "cargo test --all"})),
        Script::text("done"),
    ]);
    let answer = Answer::new(ApprovalDecision::ApproveForSession);
    let mut agent = agent_with_sandbox(provider, Mode::Ask, answer.clone(), dir.path());
    let (_, events) = run(&mut agent, "go").await;
    assert_eq!(answer.asked().len(), 1);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Warning { .. })),
        "{events:#?}"
    );
}
```

In `crates/harness-tui/Cargo.toml`:

Replace (1 of 2):

```toml
publish.workspace = true

[dependencies]
futures.workspace = true
harness-context.workspace = true
harness-core.workspace = true
```

with:

```toml
publish.workspace = true

[dependencies]
async-trait.workspace = true
futures.workspace = true
harness-context.workspace = true
harness-core.workspace = true
```

Replace (2 of 2):

```toml
unicode-width.workspace = true

[dev-dependencies]
tempfile.workspace = true
```

with:

```toml
unicode-width.workspace = true

[dev-dependencies]
harness-tools.workspace = true
tempfile.workspace = true
```

Create `crates/harness-tui/tests/approvals.rs`:

```rust
//! Approvals at the terminal, with scripted keys: the diff of a file change, approving once or
//! for the session, denying with a reason, and the offer to run a command outside the sandbox.

use std::{path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use harness_core::{
    agent::{Agent, AgentConfig},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    message::{Message, ToolSpec},
    permission::{Action, Mode},
    testing::{MockProvider, Script},
    tool::{Tool, ToolContext, ToolOutput, ToolRegistry},
};
use harness_tui::{
    app::{Host, Options, Prepared},
    approval::ChannelApprover,
    inline::InlineTerminal,
    style::Theme,
    ui::Ui,
};
use ratatui::{
    backend::TestBackend,
    buffer::Buffer,
    crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers},
};
use serde_json::{Value, json};

struct NoCommands;

impl Host for NoCommands {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
}

/// Fails as if the sandbox blocked it, unless it runs outside the sandbox.
struct Boxed;

#[async_trait]
impl Tool for Boxed {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fetch".into(),
            description: "fetch".into(),
            parameters: json!({"type": "object"}),
        }
    }
    fn action(&self, _args: &Value, _ctx: &ToolContext) -> Action {
        Action::Bash("curl https://example.com".into())
    }
    async fn run(&self, _args: Value, ctx: &ToolContext) -> ToolOutput {
        if ctx.unsandboxed {
            return ToolOutput::ok("fetched without the sandbox");
        }
        let mut out = ToolOutput::error("exit code 6\ncurl: (6) Could not resolve host");
        out.sandbox_denied = true;
        out
    }
}

fn start(provider: Arc<MockProvider>, dir: &Path, mode: Mode) -> Ui<TestBackend> {
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode,
        workspace: dir.to_path_buf(),
        read_dirs: Vec::new(),
        rules: RuleSet::default(),
        // Shell commands run directly in these tests, as if sandboxed.
        sandbox_available: true,
        writes_need_approval: false,
    }));
    let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(Boxed)];
    for name in ["read", "write", "edit", "bash"] {
        tools.push(harness_tools::builtin().get(name).unwrap());
    }
    let (approver, approvals) = ChannelApprover::new();
    let agent = Agent::new(
        provider,
        ToolRegistry::new(tools),
        policy,
        approver,
        AgentConfig::new("mock/m", "m", "system", dir.join("out")),
        ToolContext::new(dir),
    );
    let options = Options {
        theme: Theme::monochrome(),
        model: "mock/m".into(),
        mode,
        commands: Vec::new(),
        workspace: dir.to_path_buf(),
        history: Vec::new(),
        instruction_files: Vec::new(),
        window_note: None,
    };
    let term = InlineTerminal::new(TestBackend::new(80, 24), 0).unwrap();
    let mut ui = Ui::start(agent, Box::new(NoCommands), term, options, approvals);
    ui.draw().unwrap();
    ui
}

fn rows(buffer: &Buffer) -> Vec<String> {
    let width = buffer.area.width as usize;
    buffer
        .content
        .chunks(width.max(1))
        .map(|row| {
            let text: String = row.iter().map(|cell| cell.symbol()).collect();
            text.trim_end().to_string()
        })
        .collect()
}

fn screen(ui: &Ui<TestBackend>) -> Vec<String> {
    rows(ui.terminal().backend().buffer())
}

fn everything(ui: &Ui<TestBackend>) -> Vec<String> {
    let backend = ui.terminal().backend();
    let mut out = rows(backend.scrollback());
    out.extend(rows(backend.buffer()));
    out
}

fn press(ui: &mut Ui<TestBackend>, code: KeyCode) {
    ui.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        .unwrap();
}

fn send(ui: &mut Ui<TestBackend>, text: &str) {
    for c in text.chars() {
        press(ui, KeyCode::Char(c));
    }
    press(ui, KeyCode::Enter);
}

async fn until_asked(ui: &mut Ui<TestBackend>) {
    tokio::time::timeout(
        Duration::from_secs(10),
        ui.until(|app| app.prompt().is_some()),
    )
    .await
    .expect("an approval is asked for")
    .unwrap();
}

async fn settle(ui: &mut Ui<TestBackend>) {
    tokio::time::timeout(Duration::from_secs(10), ui.settle())
        .await
        .expect("the turn ends")
        .unwrap();
}

/// What the model was told about tool call `id`.
fn tool_result(provider: &MockProvider, id: &str) -> String {
    provider
        .requests()
        .iter()
        .flat_map(|r| r.messages.clone())
        .find_map(|m| match m {
            Message::Tool {
                call_id, content, ..
            } if call_id == id => Some(content),
            _ => None,
        })
        .unwrap_or_default()
}

fn edit_script(file: &str, from: &str, to: &str) -> Vec<Script> {
    vec![
        Script::tool_call("r1", "read", json!({"path": file})),
        Script::tool_call(
            "e1",
            "edit",
            json!({"path": file, "old_string": from, "new_string": to}),
        ),
        Script::text("Edited."),
    ]
}

#[tokio::test]
async fn an_edit_in_ask_mode_shows_its_diff_and_runs_once_approved() {
    let dir = tempfile::tempdir().unwrap();
    let text: String = (1..=9).map(|i| format!("line {i}\n")).collect();
    std::fs::write(dir.path().join("notes.txt"), &text).unwrap();
    let provider = MockProvider::new(edit_script("notes.txt", "line 5\n", "line five\n"));
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "fix line 5");
    until_asked(&mut ui).await;
    let shown = screen(&ui);
    let at = |text: &str| {
        shown
            .iter()
            .position(|r| r.trim_start() == text)
            .unwrap_or_else(|| panic!("no {text:?} in {shown:#?}"))
    };
    assert!(
        shown.iter().any(|r| r.starts_with("approve? write ")),
        "{shown:#?}"
    );
    assert!(shown.iter().any(|r| r.contains("edit notes.txt (+1 -1)")));
    assert!(at("@@ -2,7 +2,7 @@") < at("-line 5"));
    assert_eq!(at("-line 5") + 1, at("+line five"));
    assert!(
        shown
            .iter()
            .any(|r| r.contains("[a] yes, for this session"))
    );
    // Nothing changed yet.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("notes.txt")).unwrap(),
        text
    );
    press(&mut ui, KeyCode::Char('y'));
    settle(&mut ui).await;
    assert!(
        std::fs::read_to_string(dir.path().join("notes.txt"))
            .unwrap()
            .contains("line five")
    );
    assert!(
        everything(&ui)
            .iter()
            .any(|r| r.starts_with("✓ approved: write"))
    );
    assert!(everything(&ui).iter().any(|r| r == "Edited."));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn denying_with_a_reason_tells_the_model_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
    let provider = MockProvider::new(edit_script("a.txt", "one", "two"));
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "change it");
    until_asked(&mut ui).await;
    press(&mut ui, KeyCode::Char('n'));
    assert!(
        screen(&ui)
            .iter()
            .any(|r| r.starts_with("tell the model why"))
    );
    send(&mut ui, "keep it as it is");
    settle(&mut ui).await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "one\n"
    );
    assert_eq!(
        tool_result(&provider, "e1"),
        "the user denied this action: keep it as it is"
    );
    assert!(
        everything(&ui)
            .iter()
            .any(|r| r.contains("denied: write") && r.ends_with("(keep it as it is)"))
    );
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn a_command_approved_for_the_session_is_not_asked_about_again() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("b1", "bash", json!({"command": "echo one"})),
        Script::tool_call("b2", "bash", json!({"command": "echo two"})),
        Script::text("Both ran."),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "run them");
    until_asked(&mut ui).await;
    assert!(screen(&ui).iter().any(|r| r.trim_start() == "$ echo one"));
    press(&mut ui, KeyCode::Char('a'));
    settle(&mut ui).await;
    assert!(tool_result(&provider, "b2").contains("two"));
    let approvals = everything(&ui)
        .iter()
        .filter(|r| r.starts_with("✓ approved"))
        .count();
    assert_eq!(approvals, 1);
    assert!(everything(&ui).iter().any(|r| r == "Both ran."));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn a_blocked_command_can_run_outside_the_sandbox_once_but_never_for_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("f1", "fetch", json!({})),
        Script::text("Fetched."),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Auto);
    send(&mut ui, "fetch it");
    until_asked(&mut ui).await;
    let shown = screen(&ui).join("\n");
    assert!(
        shown.contains("the sandbox may have blocked this command"),
        "{shown}"
    );
    assert!(!shown.contains("for this session"), "{shown}");
    // `a` does nothing here.
    press(&mut ui, KeyCode::Char('a'));
    assert!(ui.app().prompt().is_some());
    press(&mut ui, KeyCode::Char('y'));
    settle(&mut ui).await;
    assert_eq!(tool_result(&provider, "f1"), "fetched without the sandbox");
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn a_long_diff_scrolls_inside_the_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let content: String = (1..=100).map(|i| format!("row {i}\n")).collect();
    let provider = MockProvider::new(vec![
        Script::tool_call(
            "w1",
            "write",
            json!({"path": "big.txt", "content": content}),
        ),
        Script::text("Written."),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "write it");
    until_asked(&mut ui).await;
    let shown = screen(&ui);
    assert!(shown.iter().any(|r| r.contains("write big.txt (+100 -0)")));
    assert!(shown.iter().any(|r| r.trim_start() == "+row 1"));
    assert!(!shown.iter().any(|r| r.trim_start() == "+row 60"));
    assert!(
        shown
            .iter()
            .any(|r| r.contains("of 102: Up, Down, PgUp and PgDn scroll")),
        "{shown:#?}"
    );
    for _ in 0..6 {
        press(&mut ui, KeyCode::PageDown);
    }
    let shown = screen(&ui);
    assert!(
        shown.iter().any(|r| r.trim_start() == "+row 100"),
        "{shown:#?}"
    );
    assert!(!shown.iter().any(|r| r.trim_start() == "+row 1"));
    press(&mut ui, KeyCode::Char('y'));
    settle(&mut ui).await;
    assert!(dir.path().join("big.txt").exists());
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn esc_at_a_prompt_denies_and_stops_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("b1", "bash", json!({"command": "echo hi"})),
        Script::text("never asked"),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "go");
    until_asked(&mut ui).await;
    press(&mut ui, KeyCode::Esc);
    settle(&mut ui).await;
    assert!(ui.app().prompt().is_none());
    assert!(
        everything(&ui)
            .iter()
            .any(|r| r.starts_with("✗ denied, and stopped"))
    );
    assert!(everything(&ui).iter().any(|r| r == "interrupted"));
    assert_eq!(provider.requests().len(), 1);
    ui.finish().await.unwrap();
}

// Review Focus: Ctrl+C at a prompt must answer it (no) and stop the turn, as Esc does; a turn
// waiting on an unanswered approval could not stop.
#[tokio::test]
async fn ctrl_c_at_a_prompt_denies_and_stops_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("b1", "bash", json!({"command": "echo hi"})),
        Script::text("never asked"),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "go");
    until_asked(&mut ui).await;
    ui.handle(Event::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )))
    .unwrap();
    settle(&mut ui).await;
    assert!(ui.app().prompt().is_none());
    assert!(
        everything(&ui)
            .iter()
            .any(|r| r.starts_with("✗ denied, and stopped"))
    );
    assert!(everything(&ui).iter().any(|r| r == "interrupted"));
    assert_eq!(provider.requests().len(), 1);
    ui.finish().await.unwrap();
}

// Review Focus: an approval that arrives while the user is typing replaces the input only while
// it waits; the draft is neither lost nor sent.
#[tokio::test]
async fn a_draft_typed_before_an_approval_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("b1", "bash", json!({"command": "echo hi"})),
        Script::text("Done."),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    send(&mut ui, "go");
    for c in "next idea".chars() {
        press(&mut ui, KeyCode::Char(c));
    }
    until_asked(&mut ui).await;
    press(&mut ui, KeyCode::Char('y'));
    settle(&mut ui).await;
    assert_eq!(ui.app().editor().text(), "next idea");
    assert_eq!(provider.requests().len(), 2);
    ui.finish().await.unwrap();
}

// Review Focus: a terminal made narrow and short still shows the prompt, wrapped, with its keys,
// and takes the answer.
#[tokio::test]
async fn a_narrowed_terminal_wraps_the_prompt_and_still_takes_the_answer() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call(
            "b1",
            "bash",
            json!({"command": "echo a-long-argument-that-does-not-fit-in-the-width"}),
        ),
        Script::text("ok"),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Ask);
    ui.terminal_mut().backend_mut().resize(16, 12);
    ui.handle(Event::Resize(16, 12)).unwrap();
    send(&mut ui, "go");
    until_asked(&mut ui).await;
    let shown = screen(&ui).join("\n");
    assert!(shown.contains("approve?"), "{shown}");
    assert!(shown.contains("[y] yes"), "{shown}");
    press(&mut ui, KeyCode::Char('y'));
    settle(&mut ui).await;
    assert!(tool_result(&provider, "b1").contains("a-long-argument"));
    ui.finish().await.unwrap();
}
```

The other sessions pass the approval requests too. In `crates/harness-tui/tests/session.rs`:

Replace (1 of 2):

```rust
};
use harness_tui::{
    app::{App, Host, Options, Prepared},
    inline::InlineTerminal,
    style::Theme,
    terminal::{Modes, RawMode},
```

with:

```rust
};
use harness_tui::{
    app::{App, Host, Options, Prepared},
    approval::ChannelApprover,
    inline::InlineTerminal,
    style::Theme,
    terminal::{Modes, RawMode},
```

Replace (2 of 2):

```rust

fn start(provider: Arc<MockProvider>, dir: &Path) -> Ui<TestBackend> {
    let term = InlineTerminal::new(TestBackend::new(60, 16), 0).unwrap();
    let mut ui = Ui::start(agent(provider, dir), Box::new(TestHost), term, options(dir));
    ui.draw().unwrap();
    ui
}
```

with:

```rust

fn start(provider: Arc<MockProvider>, dir: &Path) -> Ui<TestBackend> {
    let term = InlineTerminal::new(TestBackend::new(60, 16), 0).unwrap();
    let mut ui = Ui::start(
        agent(provider, dir),
        Box::new(TestHost),
        term,
        options(dir),
        ChannelApprover::new().1,
    );
    ui.draw().unwrap();
    ui
}
```

In `crates/harness-tui/tests/status.rs`:

Replace (1 of 2):

```rust
};
use harness_tui::{
    app::{Host, Options, Prepared},
    inline::InlineTerminal,
    status::{self, Totals},
    style::Theme,
```

with:

```rust
};
use harness_tui::{
    app::{Host, Options, Prepared},
    approval::ChannelApprover,
    inline::InlineTerminal,
    status::{self, Totals},
    style::Theme,
```

Replace (2 of 2):

```rust
        window_note: Some("assumed".into()),
    };
    let term = InlineTerminal::new(TestBackend::new(70, 20), 0).unwrap();
    let mut ui = Ui::start(agent, Box::new(NoCommands), term, options);
    ui.draw().unwrap();
    ui
}
```

with:

```rust
        window_note: Some("assumed".into()),
    };
    let term = InlineTerminal::new(TestBackend::new(70, 20), 0).unwrap();
    let mut ui = Ui::start(
        agent,
        Box::new(NoCommands),
        term,
        options,
        ChannelApprover::new().1,
    );
    ui.draw().unwrap();
    ui
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p harness-core --test approvals`
Expected: FAIL to compile: ``unresolved import `harness_core::agent::ApprovalKind` `` and ``no field `kind` on type `&ApprovalRequest` ``.

Run: `cargo test -p harness-tui --test approvals --test session --test status`
Expected: FAIL to compile: ``unresolved import `harness_tui::approval` ``, ``this function takes 4 arguments but 5 arguments were supplied`` (`Ui::start`), ``no method named `prompt` found for reference `&App` `` and ``no method named `until` found``.

- [ ] **Step 3: Approval kinds, and session approvals that apply once**

In `crates/harness-core/src/agent.rs`:

Replace (1 of 3):

```rust
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
```

with:

```rust
    }
}

/// What an approval decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalKind {
    /// Whether the action may run: once, for the rest of the session, or not.
    Action,
    /// Whether a command may run without the sandbox: once, or not. It is never approved for
    /// the session.
    RunUnsandboxed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub call_id: String,
    pub tool: String,
    pub action: Action,
    pub reason: String,
    pub kind: ApprovalKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
```

Replace (2 of 3):

```rust
                    tool: call.name.clone(),
                    action,
                    reason: reason.clone(),
                };
                match self.approver.decide(&request).await {
                    ApprovalDecision::Approve => {}
                    ApprovalDecision::ApproveForSession => {
                        self.policy.remember(&request.action);
                    }
                    ApprovalDecision::Deny {
                        feedback: Some(note),
```

with:

```rust
                    tool: call.name.clone(),
                    action,
                    reason: reason.clone(),
                    kind: ApprovalKind::Action,
                };
                match self.approver.decide(&request).await {
                    ApprovalDecision::Approve => {}
                    ApprovalDecision::ApproveForSession => {
                        if !self.policy.remember(&request.action) {
                            let _ = events.send(AgentEvent::Warning {
                                message: format!(
                                    "approved once: {reason} cannot be approved for the rest of the session, so harness will ask again next time"
                                ),
                            });
                        }
                    }
                    ApprovalDecision::Deny {
                        feedback: Some(note),
```

Replace (3 of 3):

```rust
            tool: call.name.clone(),
            action: tool.action(&args, &self.ctx),
            reason,
        };
        let note = match self.approver.decide(&request).await {
            ApprovalDecision::Approve | ApprovalDecision::ApproveForSession => {
```

with:

```rust
            tool: call.name.clone(),
            action: tool.action(&args, &self.ctx),
            reason,
            kind: ApprovalKind::RunUnsandboxed,
        };
        let note = match self.approver.decide(&request).await {
            ApprovalDecision::Approve | ApprovalDecision::ApproveForSession => {
```

Run: `cargo test -p harness-core`
Expected: PASS, including `a_session_approval_that_cannot_be_kept_says_it_applied_once`.

- [ ] **Step 4: The prompt**

Create `crates/harness-tui/src/approval.rs`:

```rust
//! Approvals at the terminal. The agent asks through [`ChannelApprover`]; the session shows the
//! request, with a diff for a file change, until the user answers: once, for the rest of the
//! session, or no, with a reason for the model if they like.

use std::{cell::Cell, io::Read, path::Path, sync::Arc};

use async_trait::async_trait;
use harness_core::{
    agent::{ApprovalDecision, ApprovalKind, ApprovalRequest, Approver},
    permission::resolve_path,
};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    text::{Line, Span},
};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::{
    diff,
    style::Theme,
    text::{lines, sanitize, wrap},
};

/// Files larger than this are not read for a diff.
const MAX_DIFF_FILE: u64 = 1024 * 1024;

/// Where the user's answer goes.
pub type Reply = oneshot::Sender<ApprovalDecision>;
/// The approvals the agent asks for, in order.
pub type Requests = mpsc::UnboundedReceiver<(ApprovalRequest, Reply)>;

/// Asks the session's user: sends each request to the terminal and waits for the answer. If the
/// session has gone, the action is denied.
pub struct ChannelApprover {
    requests: mpsc::UnboundedSender<(ApprovalRequest, Reply)>,
}

impl ChannelApprover {
    /// The approver for the agent, and the requests for the session.
    pub fn new() -> (Arc<ChannelApprover>, Requests) {
        let (requests, receiver) = mpsc::unbounded_channel();
        (Arc::new(ChannelApprover { requests }), receiver)
    }
}

#[async_trait]
impl Approver for ChannelApprover {
    async fn decide(&self, request: &ApprovalRequest) -> ApprovalDecision {
        let (reply, answer) = oneshot::channel();
        if self.requests.send((request.clone(), reply)).is_err() {
            return ApprovalDecision::Deny { feedback: None };
        }
        answer
            .await
            .unwrap_or(ApprovalDecision::Deny { feedback: None })
    }
}

/// What a key did to the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answered {
    /// The user decided.
    Decided(ApprovalDecision),
    /// No, and stop the turn.
    Interrupt,
}

/// An approval waiting for the user's answer.
pub struct Prompt {
    request: ApprovalRequest,
    reply: Option<Reply>,
    /// What would run or change: the command, or the file's diff.
    body: Vec<Line<'static>>,
    /// The first line of `body` shown.
    scroll: usize,
    /// How many lines of `body` the last render showed, for paging.
    room: Cell<usize>,
    /// While the user types why they deny.
    feedback: Option<String>,
}

impl Prompt {
    /// The prompt for `request`, whose tool call has `arguments` (when known), in `workspace`.
    pub fn new(
        request: ApprovalRequest,
        reply: Reply,
        arguments: Option<&Value>,
        workspace: &Path,
        theme: &Theme,
    ) -> Prompt {
        let body = body(&request, arguments, workspace, theme);
        Prompt {
            request,
            reply: Some(reply),
            body,
            scroll: 0,
            room: Cell::new(10),
            feedback: None,
        }
    }

    pub fn request(&self) -> &ApprovalRequest {
        &self.request
    }

    /// Sends `decision` to the agent.
    pub fn answer(&mut self, decision: ApprovalDecision) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(decision);
        }
    }

    /// Handles a key.
    pub fn key(&mut self, key: KeyEvent) -> Option<Answered> {
        let rows = self.room.get();
        if let Some(text) = &mut self.feedback {
            match key.code {
                KeyCode::Enter => {
                    let text = text.trim().to_string();
                    return Some(Answered::Decided(ApprovalDecision::Deny {
                        feedback: (!text.is_empty()).then_some(text),
                    }));
                }
                KeyCode::Esc => self.feedback = None,
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => text.push(c),
                _ => {}
            }
            return None;
        }
        let last = self.body.len().saturating_sub(rows);
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                return Some(Answered::Decided(ApprovalDecision::Approve));
            }
            KeyCode::Char('a') if self.request.kind == ApprovalKind::Action => {
                return Some(Answered::Decided(ApprovalDecision::ApproveForSession));
            }
            KeyCode::Char('n') => self.feedback = Some(String::new()),
            KeyCode::Esc => return Some(Answered::Interrupt),
            KeyCode::Down => self.scroll = (self.scroll + 1).min(last),
            KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.scroll = (self.scroll + rows.max(1)).min(last)
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(rows.max(1)),
            _ => {}
        }
        None
    }

    /// The prompt as at most `rows` lines `width` columns wide.
    pub fn render(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        let mut head = wrap(
            &Line::from(vec![
                Span::styled("approve? ", theme.warning()),
                Span::styled(
                    sanitize(&self.request.reason).replace('\n', " "),
                    theme.bold(),
                ),
            ]),
            width,
            &[],
            &[Span::raw("  ")],
        );
        let mut foot = Vec::new();
        match &self.feedback {
            Some(text) => foot.push(Line::from(vec![
                Span::styled(
                    "tell the model why (Enter to send, Esc to go back): ",
                    theme.dim(),
                ),
                Span::raw(sanitize(text)),
            ])),
            None => {
                let mut keys = vec![Span::styled("[y] ", theme.accent()), Span::raw("yes  ")];
                if self.request.kind == ApprovalKind::Action {
                    keys.push(Span::styled("[a] ", theme.accent()));
                    keys.push(Span::raw("yes, for this session  "));
                }
                keys.push(Span::styled("[n] ", theme.accent()));
                keys.push(Span::raw("no  "));
                keys.push(Span::styled("[Esc] ", theme.accent()));
                keys.push(Span::raw("no, and stop"));
                foot.extend(wrap(&Line::from(keys), width, &[], &[]));
            }
        }
        // A reason too long for the screen is cut, so the keys and some of the body show.
        let keep = rows.saturating_sub(foot.len() + 2).max(1);
        if head.len() > keep {
            head.truncate(keep);
            if let Some(last) = head.last_mut() {
                last.spans.push(Span::styled(" …", theme.dim()));
            }
        }
        let room = rows.saturating_sub(head.len() + foot.len() + 1).max(1);
        self.room.set(room);
        let shown: Vec<Line<'static>> = self
            .body
            .iter()
            .skip(self.scroll)
            .take(room)
            .flat_map(|line| wrap(line, width, &[Span::raw("  ")], &[Span::raw("  ")]))
            .take(room)
            .collect();
        head.extend(shown);
        if self.body.len() > room {
            let end = (self.scroll + room).min(self.body.len());
            head.push(Line::from(Span::styled(
                format!(
                    "  lines {}-{end} of {}: Up, Down, PgUp and PgDn scroll",
                    self.scroll + 1,
                    self.body.len()
                ),
                theme.dim(),
            )));
        }
        head.extend(foot);
        head
    }

    /// Lines for the scrollback saying how the user answered.
    pub fn outcome(&self, answered: &Answered, theme: &Theme) -> Line<'static> {
        let reason = sanitize(&self.request.reason).replace('\n', " ");
        let (mark, text) = match answered {
            Answered::Decided(ApprovalDecision::Approve) => ("✓", format!("approved: {reason}")),
            Answered::Decided(ApprovalDecision::ApproveForSession) => {
                ("✓", format!("approved for this session: {reason}"))
            }
            Answered::Decided(ApprovalDecision::Deny {
                feedback: Some(why),
            }) => ("✗", format!("denied: {reason} ({})", sanitize(why))),
            Answered::Decided(_) => ("✗", format!("denied: {reason}")),
            Answered::Interrupt => ("✗", format!("denied, and stopped: {reason}")),
        };
        Line::from(Span::styled(format!("{mark} {text}"), theme.dim()))
    }
}

/// What the prompt shows under the reason: the command, or the file's change as a diff.
fn body(
    request: &ApprovalRequest,
    arguments: Option<&Value>,
    workspace: &Path,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let text = |key: &str| {
        arguments
            .and_then(|a| a[key].as_str())
            .unwrap_or_default()
            .to_string()
    };
    match request.tool.as_str() {
        "write" | "edit" => {
            let path = text("path");
            let target = resolve_path(workspace, Path::new(&path));
            let old = match read_small(&target) {
                Ok(old) => old,
                Err(why) => {
                    return lines(&format!("{path}: {why}"), theme.dim());
                }
            };
            let new = if request.tool == "write" {
                text("content")
            } else {
                let (from, to) = (text("old_string"), text("new_string"));
                let all = arguments.is_some_and(|a| a["replace_all"].as_bool() == Some(true));
                match &old {
                    Some(old) if !from.is_empty() && old.contains(&from) => {
                        if all {
                            old.replace(&from, &to)
                        } else {
                            old.replacen(&from, &to, 1)
                        }
                    }
                    // What the model sent, when it does not match the file.
                    _ => {
                        let mut out = lines(
                            &format!("{path}: the text to replace is not in the file"),
                            theme.dim(),
                        );
                        out.extend(diff::unified(&from, &to, 3, theme));
                        return out;
                    }
                }
            };
            let old = old.unwrap_or_default();
            let (added, removed) = diff::counts(&old, &new);
            let mut out = vec![Line::from(Span::styled(
                format!("{} {} (+{added} -{removed})", request.tool, sanitize(&path)),
                theme.bold(),
            ))];
            out.extend(diff::unified(&old, &new, 3, theme));
            out
        }
        "bash" => text("command")
            .lines()
            .enumerate()
            .map(|(i, l)| {
                let marker = if i == 0 { "$ " } else { "  " };
                Line::from(vec![
                    Span::styled(marker, theme.dim()),
                    Span::raw(sanitize(l)),
                ])
            })
            .collect(),
        _ => match arguments {
            Some(args) => lines(&args.to_string(), theme.dim()),
            None => Vec::new(),
        },
    }
}

/// The text of the file at `path`: `None` when it does not exist; an error saying why when it
/// is too large or not text.
fn read_small(path: &Path) -> Result<Option<String>, String> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read it for a diff ({e})")),
    };
    let mut bytes = Vec::new();
    file.take(MAX_DIFF_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read it for a diff ({e})"))?;
    if bytes.len() as u64 > MAX_DIFF_FILE {
        return Err("too large to show a diff".into());
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| "not text, so no diff is shown".into())
}
```

In `crates/harness-tui/src/transcript.rs`:

Replace:

```rust
    /// Whether a turn is running.
    pub fn busy(&self) -> bool {
        self.busy
    }

    /// The finished lines not yet written, which are then forgotten.
```

with:

```rust
    /// Whether a turn is running.
    pub fn busy(&self) -> bool {
        self.busy
    }

    /// The arguments of tool call `id`, while it runs.
    pub fn arguments(&self, id: &str) -> Option<&Value> {
        self.calls.get(id).map(|call| &call.arguments)
    }

    /// The finished lines not yet written, which are then forgotten.
```

In `crates/harness-tui/src/app.rs`:

Replace (1 of 9):

```rust
use std::time::{Duration, Instant};

use harness_context::commands::{is_builtin, parse_invocation};
use harness_core::{agent::ContextUsage, event::AgentEvent, permission::Mode, turn::TurnInput};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::Position,
```

with:

```rust
use std::time::{Duration, Instant};

use harness_context::commands::{is_builtin, parse_invocation};
use harness_core::{
    agent::{ApprovalDecision, ApprovalRequest, ContextUsage},
    event::AgentEvent,
    permission::Mode,
    turn::TurnInput,
};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::Position,
```

Replace (2 of 9):

```rust
};

use crate::{
    complete::{self, Completer, Offer},
    editor::{Edit, Editor},
    status::{self, Totals},
```

with:

```rust
};

use crate::{
    approval::{Answered, Prompt, Reply},
    complete::{self, Completer, Offer},
    editor::{Edit, Editor},
    status::{self, Totals},
```

Replace (3 of 9):

```rust
    totals: Totals,
    instruction_files: Vec<(String, u64)>,
    window_note: Option<String>,
    width: usize,
}

```

with:

```rust
    totals: Totals,
    instruction_files: Vec<(String, u64)>,
    window_note: Option<String>,
    /// An approval waiting for the user's answer.
    prompt: Option<Prompt>,
    workspace: std::path::PathBuf,
    width: usize,
}

```

Replace (4 of 9):

```rust
            totals: Totals::default(),
            instruction_files: options.instruction_files,
            window_note: options.window_note,
            width,
        }
    }
```

with:

```rust
            totals: Totals::default(),
            instruction_files: options.instruction_files,
            window_note: options.window_note,
            prompt: None,
            workspace: options.workspace,
            width,
        }
    }
```

Replace (5 of 9):

```rust
    /// The screen's width changed.
    pub fn set_width(&mut self, width: usize) {
        self.width = width;
    }

    pub fn editor(&self) -> &Editor {
```

with:

```rust
    /// The screen's width changed.
    pub fn set_width(&mut self, width: usize) {
        self.width = width;
    }

    /// The agent asks the user to approve `request`.
    pub fn on_approval(&mut self, request: ApprovalRequest, reply: Reply) {
        let arguments = self.transcript.arguments(&request.call_id).cloned();
        let theme = self.theme();
        self.prompt = Some(Prompt::new(
            request,
            reply,
            arguments.as_ref(),
            &self.workspace,
            &theme,
        ));
    }

    /// The approval waiting for an answer.
    pub fn prompt(&self) -> Option<&Prompt> {
        self.prompt.as_ref()
    }

    /// Answers the waiting approval, and notes the answer in the transcript.
    fn answer(&mut self, answered: Answered) {
        let Some(mut prompt) = self.prompt.take() else {
            return;
        };
        let line = prompt.outcome(&answered, &self.theme());
        let decision = match answered {
            Answered::Decided(decision) => decision,
            Answered::Interrupt => ApprovalDecision::Deny {
                feedback: Some("the user stopped the turn".into()),
            },
        };
        prompt.answer(decision);
        self.transcript.push_lines(vec![line], self.width);
    }

    pub fn editor(&self) -> &Editor {
```

Replace (6 of 9):

```rust
        }
        self.ctrl_c = None;
        self.hint = None;
        if ctrl && key.code == KeyCode::Char('d') && self.editor.is_empty() {
            return Some(Action::Quit);
        }
```

with:

```rust
        }
        self.ctrl_c = None;
        self.hint = None;
        if let Some(prompt) = &mut self.prompt {
            let answered = prompt.key(key)?;
            let interrupt = answered == Answered::Interrupt;
            self.answer(answered);
            return interrupt.then_some(Action::Interrupt);
        }
        if ctrl && key.code == KeyCode::Char('d') && self.editor.is_empty() {
            return Some(Action::Quit);
        }
```

Replace (7 of 9):

```rust
        }
        self.ctrl_c = Some(now);
        self.hint = Some("press Ctrl+C again to exit".into());
        if self.busy() {
            return Some(Action::Interrupt);
        }
```

with:

```rust
        }
        self.ctrl_c = Some(now);
        self.hint = Some("press Ctrl+C again to exit".into());
        if self.prompt.is_some() {
            self.answer(Answered::Interrupt);
        }
        if self.busy() {
            return Some(Action::Interrupt);
        }
```

Replace (8 of 9):

```rust
        )
    }

    /// The live region's lines, at most `rows`, and where the cursor is in them.
    pub fn live(&self, rows: usize) -> (Vec<Line<'static>>, Position) {
        let theme = self.theme();
        let width = self.width;
        let (editor, cursor) = self.editor.render("› ", width, &theme);
        let mut below: Vec<Line<'static>> = Vec::new();
        if let Some(completion) = &self.completion {
            below.extend(complete::render(
                &completion.offer,
```

with:

```rust
        )
    }

    /// The live region's lines, at most `rows`, and where the cursor is in them (none while an
    /// approval waits).
    pub fn live(&self, rows: usize) -> (Vec<Line<'static>>, Option<Position>) {
        let theme = self.theme();
        let width = self.width;
        let mut below: Vec<Line<'static>> = Vec::new();
        below.extend(wrap(&self.status(), width, &[], &[]));
        if let Some(prompt) = &self.prompt {
            let mut lines = prompt.render(width, rows.saturating_sub(below.len()), &theme);
            lines.extend(below);
            let skip = lines.len().saturating_sub(rows);
            return (lines.split_off(skip), None);
        }
        below.clear();
        let (editor, cursor) = self.editor.render("› ", width, &theme);
        if let Some(completion) = &self.completion {
            below.extend(complete::render(
                &completion.offer,
```

Replace (9 of 9):

```rust
            cursor.x,
            (cursor.y as usize + top).saturating_sub(skip) as u16,
        );
        (lines.split_off(skip), cursor)
    }
}
```

with:

```rust
            cursor.x,
            (cursor.y as usize + top).saturating_sub(skip) as u16,
        );
        (lines.split_off(skip), Some(cursor))
    }
}
```

In `crates/harness-tui/src/ui.rs`:

Replace (1 of 8):

```rust

use crate::{
    app::{Action, App, Host, Options},
    inline::InlineTerminal,
};

```

with:

```rust

use crate::{
    app::{Action, App, Host, Options},
    approval::{Reply, Requests},
    inline::InlineTerminal,
};

```

Replace (2 of 8):

```rust
    contexts: mpsc::UnboundedReceiver<ContextUsage>,
    /// A job was sent whose context update has not come yet.
    awaiting_context: bool,
    runner: Option<JoinHandle<()>>,
    /// Cancels the running turn.
    cancel: Option<CancellationToken>,
```

with:

```rust
    contexts: mpsc::UnboundedReceiver<ContextUsage>,
    /// A job was sent whose context update has not come yet.
    awaiting_context: bool,
    approvals: Requests,
    runner: Option<JoinHandle<()>>,
    /// Cancels the running turn.
    cancel: Option<CancellationToken>,
```

Replace (3 of 8):

```rust
    B: Backend,
    B::Error: Send + Sync + 'static,
{
    /// Starts the session: `agent` moves to a task of its own.
    pub fn start(
        agent: Agent,
        host: Box<dyn Host>,
        term: InlineTerminal<B>,
        options: Options,
    ) -> Self {
        let (jobs, mut queue) = mpsc::unbounded_channel::<Job>();
        let (events_tx, events) = mpsc::unbounded_channel();
```

with:

```rust
    B: Backend,
    B::Error: Send + Sync + 'static,
{
    /// Starts the session: `agent` moves to a task of its own. `approvals` are the requests of
    /// the agent's [`ChannelApprover`](crate::approval::ChannelApprover).
    pub fn start(
        agent: Agent,
        host: Box<dyn Host>,
        term: InlineTerminal<B>,
        options: Options,
        approvals: Requests,
    ) -> Self {
        let (jobs, mut queue) = mpsc::unbounded_channel::<Job>();
        let (events_tx, events) = mpsc::unbounded_channel();
```

Replace (4 of 8):

```rust
            events,
            contexts,
            awaiting_context: false,
            runner: Some(runner),
            cancel: None,
        }
```

with:

```rust
            events,
            contexts,
            awaiting_context: false,
            approvals,
            runner: Some(runner),
            cancel: None,
        }
```

Replace (5 of 8):

```rust
            for (i, line) in lines.iter().enumerate() {
                buf.set_line(area.x, area.y + i as u16, line, area.width);
            }
            Some(ratatui::layout::Position::new(
                area.x + cursor.x,
                area.y + cursor.y,
            ))
        })
    }

```

with:

```rust
            for (i, line) in lines.iter().enumerate() {
                buf.set_line(area.x, area.y + i as u16, line, area.width);
            }
            cursor.map(|c| ratatui::layout::Position::new(area.x + c.x, area.y + c.y))
        })
    }

```

Replace (6 of 8):

```rust
        Ok(Flow::Continue)
    }

    /// Takes in where the context goes after a job, and redraws the status line.
    fn context(&mut self, context: ContextUsage) -> io::Result<Flow> {
        self.awaiting_context = false;
```

with:

```rust
        Ok(Flow::Continue)
    }

    /// Shows an approval request, after the events that came before it.
    fn approval(
        &mut self,
        request: harness_core::agent::ApprovalRequest,
        reply: Reply,
    ) -> io::Result<Flow> {
        while let Ok(event) = self.events.try_recv() {
            self.app.on_event(&event);
        }
        self.app.on_approval(request, reply);
        self.draw()?;
        Ok(Flow::Continue)
    }

    /// Takes in where the context goes after a job, and redraws the status line.
    fn context(&mut self, context: ContextUsage) -> io::Result<Flow> {
        self.awaiting_context = false;
```

Replace (7 of 8):

```rust
                None => Ok(Flow::Quit),
            },
            Some(context) = self.contexts.recv() => self.context(context),
        }
    }

    /// Takes in the agent's events until no turn is running and the runner has said where the
    /// context goes after it.
    pub async fn settle(&mut self) -> io::Result<()> {
        while self.app.busy() || self.awaiting_context || !self.events.is_empty() {
            if self.next().await? == Flow::Quit {
                break;
            }
```

with:

```rust
                None => Ok(Flow::Quit),
            },
            Some(context) = self.contexts.recv() => self.context(context),
            Some((request, reply)) = self.approvals.recv() => self.approval(request, reply),
        }
    }

    /// Takes in the agent's events until `done` holds.
    pub async fn until(&mut self, done: impl Fn(&App) -> bool) -> io::Result<()> {
        while !done(&self.app) {
            if self.next().await? == Flow::Quit {
                break;
            }
        }
        Ok(())
    }

    /// Takes in the agent's events until no turn is running and the runner has said where the
    /// context goes after it, or until an approval waits for the user.
    pub async fn settle(&mut self) -> io::Result<()> {
        while self.app.prompt().is_none()
            && (self.app.busy() || self.awaiting_context || !self.events.is_empty())
        {
            if self.next().await? == Flow::Quit {
                break;
            }
```

Replace (8 of 8):

```rust
                    None => Flow::Quit,
                },
                Some(context) = self.contexts.recv() => self.context(context)?,
            };
            if flow == Flow::Quit {
                break;
```

with:

```rust
                    None => Flow::Quit,
                },
                Some(context) = self.contexts.recv() => self.context(context)?,
                Some((request, reply)) = self.approvals.recv() => self.approval(request, reply)?,
            };
            if flow == Flow::Quit {
                break;
```

In `crates/harness-tui/src/lib.rs`:

Replace:

```rust
//! This crate draws with `ratatui`; `harness-cli` sets up the session and starts it.

pub mod app;
pub mod complete;
pub mod diff;
pub mod editor;
```

with:

```rust
//! This crate draws with `ratatui`; `harness-cli` sets up the session and starts it.

pub mod app;
pub mod approval;
pub mod complete;
pub mod diff;
pub mod editor;
```

- [ ] **Step 5: The CLI answers approvals at the terminal**

In `crates/harness-cli/src/interactive.rs`:

Replace (1 of 6):

```rust
use crossterm::event::EventStream;
use harness_config::config;
use harness_context::{commands::Commands, project::project_root};
use harness_core::{agent::NonInteractive, engine::PermissionEngine, permission::Mode};
use harness_providers::registry;
use harness_tui::{
    app::{Host, Options, Prepared},
    inline::InlineTerminal,
    style::Theme,
    terminal::{CrosstermRawMode, Modes},
```

with:

```rust
use crossterm::event::EventStream;
use harness_config::config;
use harness_context::{commands::Commands, project::project_root};
use harness_core::{engine::PermissionEngine, permission::Mode};
use harness_providers::registry;
use harness_tui::{
    app::{Host, Options, Prepared},
    approval::{ChannelApprover, Requests},
    inline::InlineTerminal,
    style::Theme,
    terminal::{CrosstermRawMode, Modes},
```

Replace (2 of 6):

```rust
    for warning in &commands.warnings {
        eprintln!("warning: {}", terminal_safe(warning));
    }
    let Started {
        agent,
        sandbox_session,
```

with:

```rust
    for warning in &commands.warnings {
        eprintln!("warning: {}", terminal_safe(warning));
    }
    let (approver, approvals) = ChannelApprover::new();
    let Started {
        agent,
        sandbox_session,
```

Replace (3 of 6):

```rust
        mode,
        model: resolved,
        session,
        approver: Arc::new(NonInteractive),
        interactive: true,
    })
    .await;
```

with:

```rust
        mode,
        model: resolved,
        session,
        approver,
        interactive: true,
    })
    .await;
```

Replace (4 of 6):

```rust
        commands,
        policy,
    };
    let result = terminal_session(agent, Box::new(host), options).await;
    sandbox_session.end();
    match result {
        Ok(()) => 0,
```

with:

```rust
        commands,
        policy,
    };
    let result = terminal_session(agent, Box::new(host), options, approvals).await;
    sandbox_session.end();
    match result {
        Ok(()) => 0,
```

Replace (5 of 6):

```rust
    agent: harness_core::agent::Agent,
    host: Box<dyn Host>,
    options: Options,
) -> std::io::Result<()> {
    // Asked before any events are read: both queries read the terminal's answer from stdin.
    let keyboard = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
```

with:

```rust
    agent: harness_core::agent::Agent,
    host: Box<dyn Host>,
    options: Options,
    approvals: Requests,
) -> std::io::Result<()> {
    // Asked before any events are read: both queries read the terminal's answer from stdin.
    let keyboard = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
```

Replace (6 of 6):

```rust
    let top = if column == 0 { row } else { row + 1 };
    let _modes = Modes::enter(std::io::stdout(), CrosstermRawMode, keyboard)?;
    let term = InlineTerminal::new(CrosstermBackend::new(std::io::stdout()), top)?;
    let ui = Ui::start(agent, host, term, options);
    ui.run(EventStream::new()).await
}

```

with:

```rust
    let top = if column == 0 { row } else { row + 1 };
    let _modes = Modes::enter(std::io::stdout(), CrosstermRawMode, keyboard)?;
    let term = InlineTerminal::new(CrosstermBackend::new(std::io::stdout()), top)?;
    let ui = Ui::start(agent, host, term, options, approvals);
    ui.run(EventStream::new()).await
}

```

- [ ] **Step 6: Run the tests, and lint**

Run: `cargo test -p harness-core -p harness-tui -p harness-cli`
Expected: PASS, including `an_edit_in_ask_mode_shows_its_diff_and_runs_once_approved`, `a_long_diff_scrolls_inside_the_prompt` and `ctrl_c_at_a_prompt_denies_and_stops_the_turn`.

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add crates/harness-core crates/harness-tui crates/harness-cli Cargo.lock
git commit -F - <<'EOF'
feat(tui): ask for approvals at the terminal, with diffs

The interactive session answers the agent's approvals: a prompt shows
the command, or the file's change as a diff that scrolls when long, and
takes y (once), a (for the rest of the session), n (with an optional
reason for the model) or Esc (no, and stop the turn). The offer to run
a command outside the sandbox is an approval of its own kind, never
given for the session. A session approval the policy cannot keep, such
as for a destructive command, now says it applied once.

<trailer lines from the controller>
EOF
```

---

### Task 9: Shift+Tab, with the sandbox for each mode

**Files:**
- Modify: `crates/harness-core/src/{permission,engine,agent}.rs`, `crates/harness-cli/src/{sandbox,start}.rs`, `crates/harness-tui/src/{app,ui}.rs`
- Test: `crates/harness-core/tests/mode_sandboxes.rs`, `crates/harness-tui/tests/modes.rs`, `sandbox.rs`'s test module

**Interfaces:**
- Consumes: P3's `Agent::set_mode`; Task 6's `App`, `Ui`.
- Produces:
  - `agent::Sandboxes { read_only, workspace_write }` with `for_mode(Mode)`, and `Agent::with_sandboxes(Sandboxes)`: `set_mode` then switches the sandbox and tells the policy whether one is available;
  - `PermissionPolicy::set_sandbox_available(bool)` (default does nothing), implemented by `PermissionEngine`;
  - `sandbox::for_modes(detected, too_broad, required) -> (Sandboxes, Option<String>)` in `harness-cli`;
  - `Action::SetMode(Mode)`, `App::take_pending_mode()`, `app::next_mode(Mode) -> Mode`.

- [ ] **Step 1: Write the failing tests**

Create `crates/harness-core/tests/mode_sandboxes.rs`:

```rust
//! Switching modes in a session whose sandbox depends on the mode: plan and read-only get a
//! read-only sandbox, ask and auto a workspace-write one (or none), full-access none.

mod common;

use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use common::run;
use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive, Sandboxes},
    engine::{EngineConfig, PermissionEngine},
    event::AgentEvent,
    message::{Message, ToolSpec},
    permission::{Action, Decision, FsAccess, Mode, PermissionPolicy},
    testing::{MockProvider, Script},
    tool::{CommandSandbox, Tool, ToolContext, ToolOutput, ToolRegistry},
};
use serde_json::{Value, json};

#[derive(Debug)]
struct Named(&'static str);

impl CommandSandbox for Named {
    fn name(&self) -> &'static str {
        self.0
    }
    fn command(
        &self,
        _access: FsAccess,
        _workspace: &Path,
        program: &str,
        _args: &[&str],
    ) -> std::io::Result<tokio::process::Command> {
        Ok(tokio::process::Command::new(program))
    }
    fn is_denial(&self, _exit_code: Option<i32>, _output: &str) -> bool {
        false
    }
}

/// A shell-like tool that reports the sandbox and access it would run with.
struct Probe;

#[async_trait]
impl Tool for Probe {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "probe".into(),
            parameters: json!({"type": "object", "properties": {"command": {"type": "string"}}}),
        }
    }
    fn action(&self, args: &Value, _ctx: &ToolContext) -> Action {
        Action::Bash(args["command"].as_str().unwrap_or("ls").to_string())
    }
    async fn run(&self, _args: Value, ctx: &ToolContext) -> ToolOutput {
        ToolOutput::ok(format!(
            "sandbox={} access={:?}",
            ctx.sandbox.as_ref().map_or("none", |s| s.name()),
            ctx.access
        ))
    }
}

fn agent(provider: Arc<MockProvider>, dir: &Path, mode: Mode, sandboxes: Sandboxes) -> Agent {
    let start = sandboxes.for_mode(mode);
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode,
        workspace: dir.to_path_buf(),
        read_dirs: vec![],
        rules: Default::default(),
        sandbox_available: start.is_some(),
        writes_need_approval: false,
    }));
    Agent::new(
        provider,
        ToolRegistry::new(vec![Arc::new(Probe)]),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m1", "m1", "system prompt", dir.join(".spill")),
        ToolContext::new(dir).with_sandbox(start, mode.fs_access()),
    )
    .with_sandboxes(sandboxes)
}

fn both() -> Sandboxes {
    Sandboxes {
        read_only: Some(Arc::new(Named("read-only box"))),
        workspace_write: Some(Arc::new(Named("write box"))),
    }
}

/// Runs `ls` through the probe and returns what it reported, or why it did not run.
async fn probe(agent: &mut Agent, provider_turn: &str) -> String {
    let (_, events) = run(agent, provider_turn).await;
    events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolCallFinished { output, .. } => Some(output.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn script(turns: usize) -> Arc<MockProvider> {
    let mut script = Vec::new();
    for i in 0..turns {
        script.push(Script::tool_call(
            &format!("c{i}"),
            "bash",
            json!({"command": "ls"}),
        ));
        script.push(Script::text("ok"));
    }
    MockProvider::new(script)
}

#[tokio::test]
async fn each_mode_gets_its_own_sandbox() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(script(4), dir.path(), Mode::Auto, both());
    assert_eq!(
        probe(&mut agent, "1").await,
        "sandbox=write box access=WorkspaceWrite"
    );
    agent.set_mode(Mode::Plan);
    assert_eq!(
        probe(&mut agent, "2").await,
        "sandbox=read-only box access=ReadOnly"
    );
    agent.set_mode(Mode::Ask);
    // `ls` is unlisted, so ask mode asks, and nobody answers here.
    assert!(
        probe(&mut agent, "3")
            .await
            .contains("no user is available")
    );
    agent.set_mode(Mode::FullAccess);
    assert_eq!(
        probe(&mut agent, "4").await,
        "sandbox=none access=WorkspaceWrite"
    );
}

#[tokio::test]
async fn leaving_full_access_brings_the_sandbox_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(script(2), dir.path(), Mode::FullAccess, both());
    assert_eq!(
        probe(&mut agent, "1").await,
        "sandbox=none access=WorkspaceWrite"
    );
    agent.set_mode(Mode::Auto);
    assert_eq!(
        probe(&mut agent, "2").await,
        "sandbox=write box access=WorkspaceWrite"
    );
}

#[tokio::test]
async fn a_mode_without_a_sandbox_asks_for_every_command_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    // A workspace too broad to make writable: only the read-only sandbox exists.
    let sandboxes = Sandboxes {
        read_only: Some(Arc::new(Named("read-only box"))),
        workspace_write: None,
    };
    let provider = script(2);
    let mut agent = agent(provider.clone(), dir.path(), Mode::Plan, sandboxes);
    assert_eq!(
        probe(&mut agent, "1").await,
        "sandbox=read-only box access=ReadOnly"
    );
    agent.set_mode(Mode::Auto);
    assert!(
        probe(&mut agent, "2")
            .await
            .contains("no user is available")
    );
    let note = provider
        .requests()
        .last()
        .unwrap()
        .messages
        .iter()
        .find_map(|m| match m {
            Message::User { content } if content.contains("approval mode is now auto") => {
                Some(content.clone())
            }
            _ => None,
        })
        .unwrap();
    assert!(note.contains("no OS sandbox is active"), "{note}");
}

#[test]
fn the_engine_follows_the_sandbox_it_is_told_about() {
    let dir = tempfile::tempdir().unwrap();
    let engine = PermissionEngine::new(EngineConfig {
        mode: Mode::Auto,
        workspace: dir.path().to_path_buf(),
        read_dirs: vec![],
        rules: Default::default(),
        sandbox_available: true,
        writes_need_approval: false,
    });
    let ls = Action::Bash("ls".into());
    assert_eq!(engine.check(&ls), Decision::Allow);
    engine.set_sandbox_available(false);
    assert!(matches!(engine.check(&ls), Decision::Ask(_)));
    engine.set_mode(Mode::Plan);
    assert!(matches!(engine.check(&ls), Decision::Deny(_)));
    engine.set_sandbox_available(true);
    assert_eq!(engine.check(&ls), Decision::Allow);
}
```

Create `crates/harness-tui/tests/modes.rs`:

```rust
//! Shift+Tab cycles the approval mode through plan, ask and auto.

use std::{path::Path, sync::Arc, time::Duration};

use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    message::Message,
    permission::Mode,
    provider::ProviderEvent,
    testing::{MockProvider, Script},
    tool::{ToolContext, ToolRegistry},
};
use harness_tui::{
    app::{Host, Options, Prepared, next_mode},
    approval::ChannelApprover,
    inline::InlineTerminal,
    style::Theme,
    ui::Ui,
};
use ratatui::{
    backend::TestBackend,
    buffer::Buffer,
    crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers},
};

struct NoCommands;

impl Host for NoCommands {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
}

fn start(provider: Arc<MockProvider>, dir: &Path, mode: Mode) -> Ui<TestBackend> {
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode,
        workspace: dir.to_path_buf(),
        read_dirs: Vec::new(),
        rules: RuleSet::default(),
        sandbox_available: false,
        writes_need_approval: false,
    }));
    let agent = Agent::new(
        provider,
        ToolRegistry::new(Vec::new()),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m", "m", "system", dir.join("out")),
        ToolContext::new(dir),
    );
    let options = Options {
        theme: Theme::monochrome(),
        model: "mock/m".into(),
        mode,
        commands: Vec::new(),
        workspace: dir.to_path_buf(),
        history: Vec::new(),
        instruction_files: Vec::new(),
        window_note: None,
    };
    let term = InlineTerminal::new(TestBackend::new(100, 20), 0).unwrap();
    let mut ui = Ui::start(
        agent,
        Box::new(NoCommands),
        term,
        options,
        ChannelApprover::new().1,
    );
    ui.draw().unwrap();
    ui
}

fn rows(buffer: &Buffer) -> Vec<String> {
    let width = buffer.area.width as usize;
    buffer
        .content
        .chunks(width.max(1))
        .map(|row| {
            let text: String = row.iter().map(|cell| cell.symbol()).collect();
            text.trim_end().to_string()
        })
        .collect()
}

fn status(ui: &Ui<TestBackend>) -> String {
    // The status line is the last row that starts with the model.
    rows(ui.terminal().backend().buffer())
        .into_iter()
        .rfind(|r| r.starts_with("mock/m · "))
        .unwrap()
}

fn press(ui: &mut Ui<TestBackend>, code: KeyCode, modifiers: KeyModifiers) {
    ui.handle(Event::Key(KeyEvent::new(code, modifiers)))
        .unwrap();
}

fn shift_tab(ui: &mut Ui<TestBackend>) {
    press(ui, KeyCode::BackTab, KeyModifiers::SHIFT);
}

async fn settle(ui: &mut Ui<TestBackend>) {
    tokio::time::timeout(Duration::from_secs(10), ui.settle())
        .await
        .expect("settles")
        .unwrap();
}

fn notes(provider: &MockProvider) -> Vec<String> {
    provider
        .requests()
        .last()
        .map(|r| {
            r.messages
                .iter()
                .filter_map(|m| match m {
                    Message::User { content } => Some(content.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn shift_tab_never_reaches_full_access_or_read_only() {
    assert_eq!(next_mode(Mode::Plan), Mode::Ask);
    assert_eq!(next_mode(Mode::Ask), Mode::Auto);
    assert_eq!(next_mode(Mode::Auto), Mode::Plan);
    assert_eq!(next_mode(Mode::ReadOnly), Mode::Ask);
    assert_eq!(next_mode(Mode::FullAccess), Mode::Plan);
}

#[tokio::test]
async fn shift_tab_switches_the_mode_and_the_agent_is_told() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::text("planning")]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Auto);
    assert!(status(&ui).starts_with("mock/m · auto ·"));
    shift_tab(&mut ui);
    settle(&mut ui).await;
    assert!(
        status(&ui).starts_with("mock/m · plan ·"),
        "{}",
        status(&ui)
    );
    for c in "look around".chars() {
        press(&mut ui, KeyCode::Char(c), KeyModifiers::NONE);
    }
    press(&mut ui, KeyCode::Enter, KeyModifiers::NONE);
    settle(&mut ui).await;
    let sent = notes(&provider).join("\n");
    assert!(
        sent.contains("[harness] The approval mode is now plan"),
        "{sent}"
    );
    assert!(
        rows(ui.terminal().backend().scrollback())
            .iter()
            .chain(rows(ui.terminal().backend().buffer()).iter())
            .any(|r| r == "switched to plan mode")
    );
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn a_mode_chosen_during_a_turn_applies_when_it_ends() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::Hang(vec![ProviderEvent::TextDelta("working".into())]),
        Script::text("next"),
    ]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Auto);
    press(&mut ui, KeyCode::Char('x'), KeyModifiers::NONE);
    press(&mut ui, KeyCode::Enter, KeyModifiers::NONE);
    tokio::time::timeout(
        Duration::from_secs(10),
        ui.until(|app| app.transcript.busy()),
    )
    .await
    .unwrap()
    .unwrap();
    shift_tab(&mut ui);
    shift_tab(&mut ui);
    let now = status(&ui);
    assert!(now.starts_with("mock/m · auto ·"), "{now}");
    assert!(now.ends_with("· ask mode after this turn"), "{now}");
    // Back to where it started: nothing to switch.
    shift_tab(&mut ui);
    assert!(!status(&ui).contains("after this turn"));
    shift_tab(&mut ui);
    press(&mut ui, KeyCode::Esc, KeyModifiers::NONE);
    settle(&mut ui).await;
    assert!(
        status(&ui).starts_with("mock/m · plan ·"),
        "{}",
        status(&ui)
    );
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn leaving_full_access_goes_to_plan_and_drops_its_warning() {
    let dir = tempfile::tempdir().unwrap();
    let mut ui = start(MockProvider::new(Vec::new()), dir.path(), Mode::FullAccess);
    assert!(status(&ui).ends_with("full-access: no sandbox, no approvals"));
    shift_tab(&mut ui);
    settle(&mut ui).await;
    let now = status(&ui);
    assert!(now.starts_with("mock/m · plan ·"), "{now}");
    assert!(!now.contains("full-access"), "{now}");
    ui.finish().await.unwrap();
}
```

In `crates/harness-cli/src/sandbox.rs`:

Replace:

```rust
        let choice = choose(None, FsAccess::WorkspaceWrite, true);
        assert!(choice.sandbox.is_none() && choice.warning.is_none());
    }
}
```

with:

```rust
        let choice = choose(None, FsAccess::WorkspaceWrite, true);
        assert!(choice.sandbox.is_none() && choice.warning.is_none());
    }

    #[test]
    fn each_access_gets_the_sandbox_it_can_use() {
        let full: Option<Arc<dyn CommandSandbox>> = Some(Arc::new(Fake(GitProtection::Full)));
        let (both, warning) = for_modes(full.clone(), false, true);
        assert!(both.read_only.is_some() && both.workspace_write.is_some() && warning.is_none());
        // Too broad to make writable: read-only only, and the startup warning is its own.
        let (broad, warning) = for_modes(full, true, false);
        assert!(broad.read_only.is_some() && broad.workspace_write.is_none() && warning.is_none());
        // The basic tier with `required`: read-only only, with the warning.
        let (basic, warning) = for_modes(basic(), false, true);
        assert!(basic.read_only.is_some() && basic.workspace_write.is_none());
        assert!(
            warning
                .unwrap()
                .contains("every shell command will need approval")
        );
        let (none, warning) = for_modes(None, false, false);
        assert!(none.read_only.is_none() && none.workspace_write.is_none() && warning.is_none());
    }
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p harness-core --test mode_sandboxes`
Expected: FAIL to compile: ``unresolved import `harness_core::agent::Sandboxes` ``, ``no method named `with_sandboxes` found for struct `Agent` `` and ``no method named `set_sandbox_available` found for struct `PermissionEngine` ``.

Run: `cargo test -p harness-tui --test modes`
Expected: FAIL to compile: ``unresolved import `harness_tui::app::next_mode` ``.

Run: `cargo test -p harness-cli --bin harness sandbox`
Expected: FAIL to compile: ``cannot find function `for_modes` in this scope``.

- [ ] **Step 3: A sandbox per mode in the core**

In `crates/harness-core/src/permission.rs`:

Replace:

```rust
    /// directly would leave commands running with the old mode's access.
    fn set_mode(&self, _mode: Mode) {}

    /// Adds rules for the current turn only, or with `None` removes them. They never override
    /// deny rules, destructive-command confirmation or the sandbox.
    fn set_turn_rules(&self, _rules: Option<RuleSet>) {}
```

with:

```rust
    /// directly would leave commands running with the old mode's access.
    fn set_mode(&self, _mode: Mode) {}

    /// Whether shell commands run in an OS sandbox from now on. Internal to the agent, like
    /// [`set_mode`](Self::set_mode), which it goes with.
    fn set_sandbox_available(&self, _available: bool) {}

    /// Adds rules for the current turn only, or with `None` removes them. They never override
    /// deny rules, destructive-command confirmation or the sandbox.
    fn set_turn_rules(&self, _rules: Option<RuleSet>) {}
```

In `crates/harness-core/src/engine.rs`:

Replace (1 of 7):

```rust
    ffi::OsStr,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::Mutex,
};

use harness_shell::{Rules, Verdict};
```

with:

```rust
    ffi::OsStr,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use harness_shell::{Rules, Verdict};
```

Replace (2 of 7):

```rust
    deny_paths: Vec<PathRule>,
    /// Config `read:`/`write:` confirm rules, expanded the same way as `deny_paths`.
    confirm_paths: Vec<PathRule>,
    sandbox_available: bool,
    writes_need_approval: bool,
    /// Where `<workspace>/.git` sends git when it is a symlink or a `gitdir:` file, resolved.
    /// Writes under it are guarded like writes under `.git`.
```

with:

```rust
    deny_paths: Vec<PathRule>,
    /// Config `read:`/`write:` confirm rules, expanded the same way as `deny_paths`.
    confirm_paths: Vec<PathRule>,
    /// Whether shell commands run in an OS sandbox in the current mode; `Agent::set_mode` may
    /// change it with the mode.
    sandbox_available: AtomicBool,
    writes_need_approval: bool,
    /// Where `<workspace>/.git` sends git when it is a symlink or a `gitdir:` file, resolved.
    /// Writes under it are guarded like writes under `.git`.
```

Replace (3 of 7):

```rust
            allow_paths,
            deny_paths,
            confirm_paths,
            sandbox_available: config.sandbox_available,
            writes_need_approval: config.writes_need_approval,
            session_bash: Mutex::new(Vec::new()),
            session_paths: Mutex::new(HashSet::new()),
```

with:

```rust
            allow_paths,
            deny_paths,
            confirm_paths,
            sandbox_available: AtomicBool::new(config.sandbox_available),
            writes_need_approval: config.writes_need_approval,
            session_bash: Mutex::new(Vec::new()),
            session_paths: Mutex::new(HashSet::new()),
```

Replace (4 of 7):

```rust

    pub fn mode(&self) -> Mode {
        *self.mode.lock().expect("mode lock")
    }

    /// Rules whose tool is not `bash`, `read`, or `write` (they never match; the CLI warns about them).
```

with:

```rust

    pub fn mode(&self) -> Mode {
        *self.mode.lock().expect("mode lock")
    }

    fn sandboxed(&self) -> bool {
        self.sandbox_available.load(Ordering::SeqCst)
    }

    /// Rules whose tool is not `bash`, `read`, or `write` (they never match; the CLI warns about them).
```

Replace (5 of 7):

```rust
                ..
            } if mode == Mode::FullAccess => Decision::Ask(reason),
            _ if mode == Mode::FullAccess => Decision::Allow,
            _ if !self.sandbox_available && matches!(mode, Mode::Plan | Mode::ReadOnly) => {
                Decision::Deny(
                    "shell commands need the OS sandbox in plan and read-only mode".into(),
                )
            }
            _ if !self.sandbox_available => Decision::Ask(format!(
                "run `{}` (no sandbox is available on this system)",
                short(command)
            )),
```

with:

```rust
                ..
            } if mode == Mode::FullAccess => Decision::Ask(reason),
            _ if mode == Mode::FullAccess => Decision::Allow,
            _ if !self.sandboxed() && matches!(mode, Mode::Plan | Mode::ReadOnly) => {
                Decision::Deny(
                    "shell commands need the OS sandbox in plan and read-only mode".into(),
                )
            }
            _ if !self.sandboxed() => Decision::Ask(format!(
                "run `{}` (no sandbox is available on this system)",
                short(command)
            )),
```

Replace (6 of 7):

```rust
    /// `glob_match` has no escape syntax, so storing it as a glob would turn the literal
    /// character into a wildcard.
    fn remember_bash(&self, command: &str) -> bool {
        if !self.sandbox_available {
            return false;
        }
        let Some(prefixes) = harness_shell::session_prefixes(command) else {
```

with:

```rust
    /// `glob_match` has no escape syntax, so storing it as a glob would turn the literal
    /// character into a wildcard.
    fn remember_bash(&self, command: &str) -> bool {
        if !self.sandboxed() {
            return false;
        }
        let Some(prefixes) = harness_shell::session_prefixes(command) else {
```

Replace (7 of 7):

```rust
        *self.mode.lock().expect("mode lock") = mode;
    }

    fn set_turn_rules(&self, rules: Option<RuleSet>) {
        let rules = rules.unwrap_or_default();
        let home = home_dir();
```

with:

```rust
        *self.mode.lock().expect("mode lock") = mode;
    }

    fn set_sandbox_available(&self, available: bool) {
        self.sandbox_available.store(available, Ordering::SeqCst);
    }

    fn set_turn_rules(&self, rules: Option<RuleSet>) {
        let rules = rules.unwrap_or_default();
        let home = home_dir();
```

In `crates/harness-core/src/agent.rs`:

Replace (1 of 6):

```rust
    retry::RetryPolicy,
    session::{Entry, EntryKind, RewindScope, Session},
    tokens::DEFAULT_CONTEXT_WINDOW,
    tool::{Tool, ToolContext, ToolOutput, ToolRegistry},
    turn::{InputPart, TurnInput, TurnModel},
};

```

with:

```rust
    retry::RetryPolicy,
    session::{Entry, EntryKind, RewindScope, Session},
    tokens::DEFAULT_CONTEXT_WINDOW,
    tool::{CommandSandbox, Tool, ToolContext, ToolOutput, ToolRegistry},
    turn::{InputPart, TurnInput, TurnModel},
};

```

Replace (2 of 6):

```rust
            retry: RetryPolicy::default(),
            context_window: DEFAULT_CONTEXT_WINDOW,
            compaction: CompactionConfig::default(),
        }
    }
}
```

with:

```rust
            retry: RetryPolicy::default(),
            context_window: DEFAULT_CONTEXT_WINDOW,
            compaction: CompactionConfig::default(),
        }
    }
}

/// The OS sandbox shell commands get in each mode, for a session whose mode can change: one for
/// read-only access (`plan`, `read-only`) and one for workspace-write access (`ask`, `auto`).
/// Either may be missing: a workspace too broad to make writable has no workspace-write sandbox,
/// and neither has a system without one. `full-access` never uses one.
#[derive(Debug, Clone, Default)]
pub struct Sandboxes {
    pub read_only: Option<Arc<dyn CommandSandbox>>,
    pub workspace_write: Option<Arc<dyn CommandSandbox>>,
}

impl Sandboxes {
    /// The sandbox for `mode`.
    pub fn for_mode(&self, mode: Mode) -> Option<Arc<dyn CommandSandbox>> {
        match mode {
            Mode::Plan | Mode::ReadOnly => self.read_only.clone(),
            Mode::Ask | Mode::Auto => self.workspace_write.clone(),
            Mode::FullAccess => None,
        }
    }
}
```

Replace (3 of 6):

```rust
    auto_compaction_paused: bool,
    /// The current turn's model calls, for its stats.
    stats: Stats,
}

impl Agent {
```

with:

```rust
    auto_compaction_paused: bool,
    /// The current turn's model calls, for its stats.
    stats: Stats,
    /// The sandbox for each mode, when switching modes also switches the sandbox.
    sandboxes: Option<Sandboxes>,
}

impl Agent {
```

Replace (4 of 6):

```rust
            turn_model: None,
            auto_compaction_paused: false,
            stats: Stats::default(),
        }
    }

```

with:

```rust
            turn_model: None,
            auto_compaction_paused: false,
            stats: Stats::default(),
            sandboxes: None,
        }
    }

```

Replace (5 of 6):

```rust
                "harness stopped before {answered} tool call(s) in this conversation finished; the model is told their effects are unknown"
            ));
        }
    }

    /// Snapshots the workspace before each turn's first change, so it can be rewound.
```

with:

```rust
                "harness stopped before {answered} tool call(s) in this conversation finished; the model is told their effects are unknown"
            ));
        }
    }

    /// Gives shell commands the sandbox for the mode whenever the mode changes
    /// ([`set_mode`](Self::set_mode)). Without it, a mode change keeps the sandbox the agent
    /// started with.
    pub fn with_sandboxes(mut self, sandboxes: Sandboxes) -> Self {
        self.sandboxes = Some(sandboxes);
        self
    }

    /// Snapshots the workspace before each turn's first change, so it can be rewound.
```

Replace (6 of 6):

```rust
        self.invalid_calls
    }

    /// Switches the approval mode between turns. The system prompt stays as it is, so providers
    /// keep reusing their prompt caches; the change is appended to the conversation as a note.
    pub fn set_mode(&mut self, mode: Mode) {
        self.policy.set_mode(mode);
        self.ctx.access = mode.fs_access();
        self.record(
```

with:

```rust
        self.invalid_calls
    }

    /// Switches the approval mode between turns, and with [`with_sandboxes`](Self::with_sandboxes)
    /// the sandbox with it. The system prompt stays as it is, so providers keep reusing their
    /// prompt caches; the change is appended to the conversation as a note.
    pub fn set_mode(&mut self, mode: Mode) {
        if let Some(sandboxes) = &self.sandboxes {
            self.ctx.sandbox = sandboxes.for_mode(mode);
            self.policy
                .set_sandbox_available(self.ctx.sandbox.is_some());
        }
        self.policy.set_mode(mode);
        self.ctx.access = mode.fs_access();
        self.record(
```

Run: `cargo test -p harness-core`
Expected: PASS, including `each_mode_gets_its_own_sandbox` and the mode-change tests from P3.

- [ ] **Step 4: The CLI looks for a sandbox whatever the mode**

In `crates/harness-cli/src/sandbox.rs`:

Replace (1 of 2):

```rust
use std::sync::Arc;

use harness_core::{
    permission::FsAccess,
    tool::{CommandSandbox, GitProtection},
};
```

with:

```rust
use std::sync::Arc;

use harness_core::{
    agent::Sandboxes,
    permission::FsAccess,
    tool::{CommandSandbox, GitProtection},
};
```

Replace (2 of 2):

```rust
            "user namespaces are unavailable ({reason}), so the sandbox can only check git hooks and config after each command; run `harness sandbox doctor` to see how to enable them"
        )),
    }
}

#[cfg(test)]
```

with:

```rust
            "user namespaces are unavailable ({reason}), so the sandbox can only check git hooks and config after each command; run `harness sandbox doctor` to see how to enable them"
        )),
    }
}

/// The sandbox for each mode from the detected one, and the warning for the modes that write:
/// read-only access keeps it (in the Linux basic tier too, since a read-only sandbox protects git
/// metadata completely), and workspace-write access gets none in a workspace too broad to make
/// writable (`too_broad`), or as [`choose`] says.
pub fn for_modes(
    detected: Option<Arc<dyn CommandSandbox>>,
    too_broad: bool,
    required: bool,
) -> (Sandboxes, Option<String>) {
    let write = if too_broad {
        Choice {
            sandbox: None,
            warning: None,
        }
    } else {
        choose(detected.clone(), FsAccess::WorkspaceWrite, required)
    };
    let sandboxes = Sandboxes {
        read_only: choose(detected, FsAccess::ReadOnly, required).sandbox,
        workspace_write: write.sandbox,
    };
    (sandboxes, write.warning)
}

#[cfg(test)]
```

In `crates/harness-cli/src/start.rs`:

Replace (1 of 6):

```rust
        std::process::id()
    );
    let output_dir = setup.paths.state_dir.join("tool-output").join(run_id);
    let sandbox_disabled_by_env =
        std::env::var("HARNESS_SANDBOX").as_deref() == Ok("none") && mode != Mode::FullAccess;
    // Write access to `/`, `$HOME` or an ancestor of it would cover the user's dotfiles. Plan and
    // read-only modes keep their read-only sandbox.
    let workspace_too_broad = mode.fs_access() == FsAccess::WorkspaceWrite
        && harness_sandbox::workspace_is_too_broad(&setup.workspace);
    let required = setup.config.linux_git_protection == LinuxGitProtection::Required;
    let settings = harness_sandbox::SandboxSettings {
        extra_writable: setup.config.writable_roots.clone(),
```

with:

```rust
        std::process::id()
    );
    let output_dir = setup.paths.state_dir.join("tool-output").join(run_id);
    let env_says_none = std::env::var("HARNESS_SANDBOX").as_deref() == Ok("none");
    let sandbox_disabled_by_env = env_says_none && mode != Mode::FullAccess;
    // Write access to `/`, `$HOME` or an ancestor of it would cover the user's dotfiles. Plan and
    // read-only modes keep their read-only sandbox.
    let workspace_too_broad = harness_sandbox::workspace_is_too_broad(&setup.workspace);
    let write_too_broad = mode.fs_access() == FsAccess::WorkspaceWrite && workspace_too_broad;
    let required = setup.config.linux_git_protection == LinuxGitProtection::Required;
    let settings = harness_sandbox::SandboxSettings {
        extra_writable: setup.config.writable_roots.clone(),
```

Replace (2 of 6):

```rust
        quarantine_dir: Some(setup.paths.data_dir.join("quarantine")),
        require_full_git_protection: required,
    };
    let detected = if mode == Mode::FullAccess || sandbox_disabled_by_env || workspace_too_broad {
        None
    } else {
        harness_sandbox::detect(settings.clone())
    };
    let choice = sandbox::choose(detected, mode.fs_access(), required);
    if let Some(warning) = &choice.warning {
        eprintln!("warning: {}", terminal_safe(warning));
    }
    let sandbox = choice.sandbox;
    // From here on, however the run is left, the sandbox's session ends: on Linux that ends what
    // sandboxed commands left running, and checks git metadata once more.
    let sandbox_session = SessionEnd::new(sandbox.clone());
    let sandboxed = sandbox.is_some();
    if mode != Mode::FullAccess && !sandboxed && choice.warning.is_none() {
        if sandbox_disabled_by_env {
            eprintln!(
                "warning: the sandbox is disabled by HARNESS_SANDBOX=none; every shell command will need approval"
            );
        } else if workspace_too_broad {
            eprintln!(
                "warning: the workspace {} is your home directory or above, where the sandbox would make your dotfiles writable, so it is off; every shell command will need approval",
                terminal_safe(&setup.workspace.display().to_string())
```

with:

```rust
        quarantine_dir: Some(setup.paths.data_dir.join("quarantine")),
        require_full_git_protection: required,
    };
    // `harness ask` needs a sandbox for its one mode only. The interactive session looks for one
    // in every mode, since the user may switch to a mode that uses it.
    let look = if interactive {
        !env_says_none
    } else {
        !(mode == Mode::FullAccess || sandbox_disabled_by_env || write_too_broad)
    };
    let detected = if look {
        harness_sandbox::detect(settings.clone())
    } else {
        None
    };
    let (sandboxes, write_warning) =
        sandbox::for_modes(detected.clone(), workspace_too_broad, required);
    let sandbox = sandboxes.for_mode(mode);
    // Only a mode that writes through the sandbox has anything to warn about.
    let warning = write_warning.filter(|_| matches!(mode, Mode::Ask | Mode::Auto));
    if let Some(warning) = &warning {
        eprintln!("warning: {}", terminal_safe(warning));
    }
    // From here on, however the run is left, the sandbox's session ends: on Linux that ends what
    // sandboxed commands left running, and checks git metadata once more.
    let session_sandbox = if interactive {
        detected
    } else {
        sandbox.clone()
    };
    let sandbox_session = SessionEnd::new(session_sandbox.clone());
    let sandboxed = sandbox.is_some();
    if mode != Mode::FullAccess && !sandboxed && warning.is_none() {
        if sandbox_disabled_by_env {
            eprintln!(
                "warning: the sandbox is disabled by HARNESS_SANDBOX=none; every shell command will need approval"
            );
        } else if write_too_broad {
            eprintln!(
                "warning: the workspace {} is your home directory or above, where the sandbox would make your dotfiles writable, so it is off; every shell command will need approval",
                terminal_safe(&setup.workspace.display().to_string())
```

Replace (3 of 6):

```rust
        );
    }
    let ctx = tool_context(&setup.workspace, sandbox, mode.fs_access()).await;
    let mut config = AgentConfig::new(
        resolved.id.clone(),
        resolved.model.clone(),
```

with:

```rust
        );
    }
    let ctx = tool_context(&setup.workspace, sandbox, mode.fs_access()).await;
    // A session that starts without a sandbox may switch to a mode that uses one.
    if ctx.sandbox.is_none()
        && let Some(sandbox) = session_sandbox
    {
        start_sandbox_session(&ctx.workspace, sandbox).await;
    }
    let mut config = AgentConfig::new(
        resolved.id.clone(),
        resolved.model.clone(),
```

Replace (4 of 6):

```rust
        Vec::new()
    };
    let checkpoints = crate::sessions::checkpoints(setup, &session, &writable);
    let agent = Agent::new(
        resolved.provider,
        harness_tools::builtin(),
        policy.clone(),
```

with:

```rust
        Vec::new()
    };
    let checkpoints = crate::sessions::checkpoints(setup, &session, &writable);
    let mut agent = Agent::new(
        resolved.provider,
        harness_tools::builtin(),
        policy.clone(),
```

Replace (5 of 6):

```rust
    )
    .with_session(session)
    .with_checkpoints(checkpoints);
    Started {
        agent,
        sandbox_session,
```

with:

```rust
    )
    .with_session(session)
    .with_checkpoints(checkpoints);
    if interactive {
        agent = agent.with_sandboxes(sandboxes);
    }
    Started {
        agent,
        sandbox_session,
```

Replace (6 of 6):

```rust
) -> ToolContext {
    let ctx = ToolContext::new(workspace).with_sandbox(sandbox, access);
    if let Some(sandbox) = ctx.sandbox.clone() {
        let workspace = ctx.workspace.clone();
        // It may walk the whole workspace. Should it fail, the first command reads what it needs.
        let _ = tokio::task::spawn_blocking(move || sandbox.start_session(&workspace)).await;
    }
    ctx
}

#[cfg(test)]
```

with:

```rust
) -> ToolContext {
    let ctx = ToolContext::new(workspace).with_sandbox(sandbox, access);
    if let Some(sandbox) = ctx.sandbox.clone() {
        start_sandbox_session(&ctx.workspace, sandbox).await;
    }
    ctx
}

/// Starts `sandbox`'s session for `workspace`, off the async runtime: it may walk the whole
/// workspace. Should it fail, the first command reads what it needs.
async fn start_sandbox_session(workspace: &Path, sandbox: Arc<dyn CommandSandbox>) {
    let workspace = workspace.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || sandbox.start_session(&workspace)).await;
}

#[cfg(test)]
```

- [ ] **Step 5: Shift+Tab**

In `crates/harness-tui/src/app.rs`:

Replace (1 of 7):

```rust
pub enum Action {
    /// Start a turn.
    Run(TurnInput),
    /// Stop the running turn.
    Interrupt,
    /// Leave harness.
```

with:

```rust
pub enum Action {
    /// Start a turn.
    Run(TurnInput),
    /// Switch the approval mode.
    SetMode(Mode),
    /// Stop the running turn.
    Interrupt,
    /// Leave harness.
```

Replace (2 of 7):

```rust
    window_note: Option<String>,
    /// An approval waiting for the user's answer.
    prompt: Option<Prompt>,
    workspace: std::path::PathBuf,
    width: usize,
}
```

with:

```rust
    window_note: Option<String>,
    /// An approval waiting for the user's answer.
    prompt: Option<Prompt>,
    /// The mode chosen while a turn runs, to switch to when it ends.
    pending_mode: Option<Mode>,
    workspace: std::path::PathBuf,
    width: usize,
}
```

Replace (3 of 7):

```rust
            instruction_files: options.instruction_files,
            window_note: options.window_note,
            prompt: None,
            workspace: options.workspace,
            width,
        }
```

with:

```rust
            instruction_files: options.instruction_files,
            window_note: options.window_note,
            prompt: None,
            pending_mode: None,
            workspace: options.workspace,
            width,
        }
```

Replace (4 of 7):

```rust
        match key.code {
            KeyCode::Esc if self.busy() => return Some(Action::Interrupt),
            KeyCode::Esc => return None,
            _ => {}
        }
        match self.editor.key(key) {
```

with:

```rust
        match key.code {
            KeyCode::Esc if self.busy() => return Some(Action::Interrupt),
            KeyCode::Esc => return None,
            KeyCode::BackTab => return self.cycle_mode(),
            _ => {}
        }
        match self.editor.key(key) {
```

Replace (5 of 7):

```rust
            Edit::Ignored => {}
        }
        None
    }

    /// Ctrl+C: interrupts a running turn, or clears the input; pressed again within
```

with:

```rust
            Edit::Ignored => {}
        }
        None
    }

    /// Shift+Tab: the next of plan, ask and auto, now, or when the running turn ends.
    fn cycle_mode(&mut self) -> Option<Action> {
        let next = next_mode(self.pending_mode.unwrap_or(self.mode));
        if self.busy() {
            self.pending_mode = (next != self.mode).then_some(next);
            return None;
        }
        self.switch_mode(next)
    }

    fn switch_mode(&mut self, mode: Mode) -> Option<Action> {
        self.mode = mode;
        self.transcript
            .push_note(&format!("switched to {mode} mode"), self.width);
        Some(Action::SetMode(mode))
    }

    /// The mode chosen during the turn that just ended, to switch to now.
    pub fn take_pending_mode(&mut self) -> Option<Action> {
        if self.busy() {
            return None;
        }
        let mode = self.pending_mode.take()?;
        self.switch_mode(mode)
    }

    /// Ctrl+C: interrupts a running turn, or clears the input; pressed again within
```

Replace (6 of 7):

```rust

    /// The status line.
    fn status(&self) -> Line<'static> {
        status::status_line(
            &self.model,
            self.mode,
            &self.context,
            &self.totals,
            &self.theme(),
        )
    }

    /// The live region's lines, at most `rows`, and where the cursor is in them (none while an
```

with:

```rust

    /// The status line.
    fn status(&self) -> Line<'static> {
        let mut line = status::status_line(
            &self.model,
            self.mode,
            &self.context,
            &self.totals,
            &self.theme(),
        );
        if let Some(next) = self.pending_mode {
            line.spans.push(Span::styled(
                format!(" · {next} mode after this turn"),
                self.theme().accent(),
            ));
        }
        line
    }

    /// The live region's lines, at most `rows`, and where the cursor is in them (none while an
```

Replace (7 of 7):

```rust
        (lines.split_off(skip), Some(cursor))
    }
}
```

with:

```rust
        (lines.split_off(skip), Some(cursor))
    }
}

/// The mode Shift+Tab switches to from `mode`: plan, ask and auto in turn. From read-only it
/// goes to ask, and from full-access to plan; it never goes to either.
pub fn next_mode(mode: Mode) -> Mode {
    match mode {
        Mode::Plan | Mode::ReadOnly => Mode::Ask,
        Mode::Ask => Mode::Auto,
        Mode::Auto | Mode::FullAccess => Mode::Plan,
    }
}
```

In `crates/harness-tui/src/ui.rs`:

Replace (1 of 4):

```rust
        input: TurnInput,
        cancel: CancellationToken,
    },
}

/// The interactive session on a terminal.
```

with:

```rust
        input: TurnInput,
        cancel: CancellationToken,
    },
    SetMode(harness_core::permission::Mode),
}

/// The interactive session on a terminal.
```

Replace (2 of 4):

```rust
                    Job::Turn { input, cancel } => {
                        agent.run_turn(input, &events_tx, cancel).await;
                    }
                }
                let _ = contexts_tx.send(agent.context_usage());
            }
```

with:

```rust
                    Job::Turn { input, cancel } => {
                        agent.run_turn(input, &events_tx, cancel).await;
                    }
                    Job::SetMode(mode) => agent.set_mode(mode),
                }
                let _ = contexts_tx.send(agent.context_usage());
            }
```

Replace (3 of 4):

```rust
                }
                Flow::Continue
            }
            Action::Interrupt => {
                if let Some(cancel) = &self.cancel {
                    cancel.cancel();
```

with:

```rust
                }
                Flow::Continue
            }
            Action::SetMode(mode) => {
                if let Some(jobs) = &self.jobs {
                    let _ = jobs.send(Job::SetMode(mode));
                    self.awaiting_context = true;
                }
                Flow::Continue
            }
            Action::Interrupt => {
                if let Some(cancel) = &self.cancel {
                    cancel.cancel();
```

Replace (4 of 4):

```rust
        Ok(flow)
    }

    /// Takes in an event from the agent, and the others already waiting.
    fn agent_event(&mut self, event: AgentEvent) -> io::Result<Flow> {
        self.app.on_event(&event);
        while let Ok(event) = self.events.try_recv() {
            self.app.on_event(&event);
        }
        self.draw()?;
        Ok(Flow::Continue)
```

with:

```rust
        Ok(flow)
    }

    /// Takes in an event from the agent, and the others already waiting; once a turn has
    /// ended, switches to the mode chosen during it.
    fn agent_event(&mut self, event: AgentEvent) -> io::Result<Flow> {
        self.app.on_event(&event);
        while let Ok(event) = self.events.try_recv() {
            self.app.on_event(&event);
        }
        if let Some(action) = self.app.take_pending_mode() {
            self.dispatch(action);
        }
        self.draw()?;
        Ok(Flow::Continue)
```

- [ ] **Step 6: Run the tests, and lint**

Run: `cargo test -p harness-core -p harness-tui -p harness-cli`
Expected: PASS; `sandbox_e2e` and `ask_e2e` unchanged.

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add crates/harness-core crates/harness-cli crates/harness-tui
git commit -F - <<'EOF'
feat(tui): cycle plan, ask and auto with Shift+Tab

Shift+Tab switches the approval mode between turns (a switch during a
turn waits for it to end), never into full-access. The agent now takes
the sandbox for each mode (Sandboxes): plan and read-only keep a
read-only sandbox even where ask and auto have none, and full-access
has none, so switching modes also switches what shell commands run in.
The interactive session looks for a sandbox whatever mode it starts in.

<trailer lines from the controller>
EOF
```

---

### Task 10: Steering: queued and send-now input

**Files:**
- Modify: `crates/harness-core/src/{turn,event,agent}.rs`, `crates/harness-tui/src/{app,transcript,ui}.rs`
- Test: `crates/harness-core/tests/steering.rs`, `crates/harness-tui/tests/steering.rs`, `crates/harness-tui/tests/session.rs`

**Interfaces:**
- Consumes: Task 6's `App`, `Ui`; the agent loop's tool-result boundary.
- Produces:
  - `turn::Steering` (a cloneable handle: `new`, `send(text)`, `take() -> Vec<String>`, `is_empty`) and `Agent::with_steering(Steering)`: what was sent joins the conversation as a user message after the step's tool results, reported as `AgentEvent::Steered { text }`;
  - `App::steering()` (the `Ui` gives it to the agent), `App::next_queued() -> Option<Action>`; Enter during a turn queues, Ctrl+S sends now.

- [ ] **Step 1: Write the failing tests**

Create `crates/harness-core/tests/steering.rs`:

```rust
//! Input the user sends while a turn runs ("send now") reaches the model with the next tool
//! results of that turn.

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use common::{Echo, run};
use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    engine::{EngineConfig, PermissionEngine},
    event::AgentEvent,
    message::{Message, ToolSpec},
    permission::{Action, Mode},
    testing::{MockProvider, Script},
    tool::{Tool, ToolContext, ToolOutput, ToolRegistry},
    turn::Steering,
};
use serde_json::{Value, json};

/// A long-running tool, during which the user sends input, and maybe stops the turn.
struct Tests(Steering, Option<tokio_util::sync::CancellationToken>);

#[async_trait]
impl Tool for Tests {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_tests".into(),
            description: "runs the tests".into(),
            parameters: json!({"type": "object"}),
        }
    }
    fn action(&self, _args: &Value, ctx: &ToolContext) -> Action {
        Action::Read(ctx.workspace.clone())
    }
    async fn run(&self, _args: Value, _ctx: &ToolContext) -> ToolOutput {
        self.0.send("use the v2 API instead");
        if let Some(stop) = &self.1 {
            stop.cancel();
        }
        ToolOutput::ok("3 tests failed")
    }
}

fn agent(provider: Arc<MockProvider>, dir: &std::path::Path, steering: &Steering) -> Agent {
    agent_stopping(provider, dir, steering, None)
}

fn agent_stopping(
    provider: Arc<MockProvider>,
    dir: &std::path::Path,
    steering: &Steering,
    stop: Option<tokio_util::sync::CancellationToken>,
) -> Agent {
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode: Mode::Auto,
        workspace: dir.to_path_buf(),
        read_dirs: vec![],
        rules: Default::default(),
        sandbox_available: false,
        writes_need_approval: false,
    }));
    Agent::new(
        provider,
        ToolRegistry::new(vec![
            Arc::new(Tests(steering.clone(), stop)),
            Arc::new(Echo),
        ]),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m1", "m1", "system prompt", dir.join(".spill")),
        ToolContext::new(dir),
    )
    .with_steering(steering.clone())
}

#[tokio::test]
async fn input_sent_now_goes_with_the_next_tool_result_in_the_same_turn() {
    let dir = tempfile::tempdir().unwrap();
    let steering = Steering::new();
    let provider = MockProvider::new(vec![
        Script::tool_call("t1", "run_tests", json!({})),
        Script::text("Switching to the v2 API."),
    ]);
    let mut agent = agent(provider.clone(), dir.path(), &steering);
    let (_, events) = run(&mut agent, "fix the tests").await;
    let second = &provider.requests()[1].messages;
    let n = second.len();
    assert!(
        matches!(&second[n - 2], Message::Tool { call_id, .. } if call_id == "t1"),
        "{second:#?}"
    );
    assert_eq!(
        second[n - 1],
        Message::User {
            content: "use the v2 API instead".into()
        }
    );
    // Reported where it happened: after the tool finished, before the next reply.
    let steered = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Steered { text } if text == "use the v2 API instead"))
        .expect("a steered event");
    let finished = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolCallFinished { .. }))
        .unwrap();
    let replied = events
        .iter()
        .position(
            |e| matches!(e, AgentEvent::AssistantMessage { content, .. } if content.contains("v2")),
        )
        .unwrap();
    assert!(finished < steered && steered < replied);
    assert!(steering.is_empty());
    // The session keeps it as a user message of that turn.
    assert!(
        agent
            .rewind_points()
            .iter()
            .any(|p| p.text == "use the v2 API instead")
    );
}

#[tokio::test]
async fn input_sent_after_the_last_tool_result_waits_for_the_frontend() {
    let dir = tempfile::tempdir().unwrap();
    let steering = Steering::new();
    let provider = MockProvider::new(vec![Script::text("No tools needed.")]);
    let mut agent = agent(provider.clone(), dir.path(), &steering);
    steering.send("too late for this turn");
    let (_, events) = run(&mut agent, "hi").await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Steered { .. }))
    );
    // Nothing took it: the frontend sends it as the next turn.
    assert_eq!(steering.take(), ["too late for this turn"]);
}

#[tokio::test]
async fn an_interrupted_turn_leaves_what_was_sent_for_the_frontend() {
    let dir = tempfile::tempdir().unwrap();
    let steering = Steering::new();
    let provider = MockProvider::new(vec![
        Script::tool_call("t1", "run_tests", json!({})),
        Script::text("never asked"),
    ]);
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut agent = agent_stopping(
        provider.clone(),
        dir.path(),
        &steering,
        Some(cancel.clone()),
    );
    let (reason, events) = common::run_with(&mut agent, "go", cancel).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Steered { .. }))
    );
    assert_eq!(reason, harness_core::event::TurnEndReason::Interrupted);
    assert_eq!(steering.take(), ["use the v2 API instead"]);
}
```

Create `crates/harness-tui/tests/steering.rs`:

```rust
//! Input typed while a turn runs: Enter queues it for when the turn ends, Ctrl+S sends it with
//! the next tool results.

use std::{path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use futures::StreamExt;
use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    message::{ChatRequest, Message, ToolSpec},
    permission::{Action, Mode},
    provider::{Provider, ProviderStream},
    testing::{MockProvider, Script},
    tool::{Tool, ToolContext, ToolOutput, ToolRegistry},
};
use harness_tui::{
    app::{Host, Options, Prepared},
    approval::ChannelApprover,
    inline::InlineTerminal,
    style::Theme,
    ui::Ui,
};
use ratatui::{
    backend::TestBackend,
    buffer::Buffer,
    crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers},
};
use serde_json::{Value, json};
use tokio::sync::Notify;

struct NoCommands;

impl Host for NoCommands {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
}

/// Runs "the tests" until the test lets it finish, or the turn is interrupted.
struct Tests(Arc<Notify>);

#[async_trait]
impl Tool for Tests {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_tests".into(),
            description: "runs the tests".into(),
            parameters: json!({"type": "object"}),
        }
    }
    fn action(&self, _args: &Value, ctx: &ToolContext) -> Action {
        Action::Read(ctx.workspace.clone())
    }
    async fn run(&self, _args: Value, ctx: &ToolContext) -> ToolOutput {
        tokio::select! {
            _ = self.0.notified() => ToolOutput::ok("3 tests failed"),
            _ = ctx.cancel.cancelled() => ToolOutput::error("interrupted"),
        }
    }
}

/// Answers as `inner` does, the first request only once the test opens `gate`.
struct Gated {
    inner: Arc<MockProvider>,
    gate: Arc<Notify>,
    first: std::sync::atomic::AtomicBool,
}

impl Provider for Gated {
    fn stream(&self, request: ChatRequest) -> ProviderStream {
        let inner = self.inner.stream(request);
        if !self.first.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return inner;
        }
        let gate = self.gate.clone();
        Box::pin(
            futures::stream::once(async move {
                gate.notified().await;
                inner
            })
            .flatten(),
        )
    }
}

fn start(provider: Arc<MockProvider>, dir: &Path, gate: &Arc<Notify>) -> Ui<TestBackend> {
    start_with(provider, dir, gate)
}

fn start_with(provider: Arc<dyn Provider>, dir: &Path, gate: &Arc<Notify>) -> Ui<TestBackend> {
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode: Mode::Auto,
        workspace: dir.to_path_buf(),
        read_dirs: Vec::new(),
        rules: RuleSet::default(),
        sandbox_available: false,
        writes_need_approval: false,
    }));
    let agent = Agent::new(
        provider,
        ToolRegistry::new(vec![Arc::new(Tests(gate.clone()))]),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m", "m", "system", dir.join("out")),
        ToolContext::new(dir),
    );
    let options = Options {
        theme: Theme::monochrome(),
        model: "mock/m".into(),
        mode: Mode::Auto,
        commands: Vec::new(),
        workspace: dir.to_path_buf(),
        history: Vec::new(),
        instruction_files: Vec::new(),
        window_note: None,
    };
    let term = InlineTerminal::new(TestBackend::new(80, 24), 0).unwrap();
    let mut ui = Ui::start(
        agent,
        Box::new(NoCommands),
        term,
        options,
        ChannelApprover::new().1,
    );
    ui.draw().unwrap();
    ui
}

fn rows(buffer: &Buffer) -> Vec<String> {
    let width = buffer.area.width as usize;
    buffer
        .content
        .chunks(width.max(1))
        .map(|row| {
            let text: String = row.iter().map(|cell| cell.symbol()).collect();
            text.trim_end().to_string()
        })
        .collect()
}

fn everything(ui: &Ui<TestBackend>) -> Vec<String> {
    let backend = ui.terminal().backend();
    let mut out = rows(backend.scrollback());
    out.extend(rows(backend.buffer()));
    out
}

fn position(ui: &Ui<TestBackend>, row: &str) -> usize {
    everything(ui)
        .iter()
        .position(|r| r == row)
        .unwrap_or_else(|| panic!("no {row:?} in {:#?}", everything(ui)))
}

fn type_text(ui: &mut Ui<TestBackend>, text: &str) {
    for c in text.chars() {
        ui.handle(Event::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )))
        .unwrap();
    }
}

fn enter(ui: &mut Ui<TestBackend>) {
    ui.handle(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
    .unwrap();
}

fn ctrl_s(ui: &mut Ui<TestBackend>) {
    ui.handle(Event::Key(KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL,
    )))
    .unwrap();
}

async fn while_the_tests_run(ui: &mut Ui<TestBackend>) {
    tokio::time::timeout(
        Duration::from_secs(10),
        ui.until(|app| app.transcript.arguments("t1").is_some()),
    )
    .await
    .expect("the tool starts")
    .unwrap();
}

async fn settle(ui: &mut Ui<TestBackend>) {
    tokio::time::timeout(Duration::from_secs(10), ui.settle())
        .await
        .expect("the turns end")
        .unwrap();
}

fn user_messages(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::User { content } => Some(content.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn ctrl_s_sends_with_the_next_tool_result_and_enter_queues_a_new_turn() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(Notify::new());
    let provider = MockProvider::new(vec![
        Script::tool_call("t1", "run_tests", json!({})),
        Script::text("Switched to the v2 API."),
        Script::text("README updated."),
    ]);
    let mut ui = start(provider.clone(), dir.path(), &gate);
    type_text(&mut ui, "fix the tests");
    enter(&mut ui);
    while_the_tests_run(&mut ui).await;
    type_text(&mut ui, "use the v2 API instead");
    ctrl_s(&mut ui);
    type_text(&mut ui, "also update the README");
    enter(&mut ui);
    let waiting = rows(ui.terminal().backend().buffer());
    assert!(
        waiting.contains(&"sending with the next tool results: use the v2 API instead".into()),
        "{waiting:#?}"
    );
    assert!(waiting.contains(&"queued: also update the README".into()));
    gate.notify_one();
    settle(&mut ui).await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    // The steering message follows the tool result in the same turn.
    let second = &requests[1].messages;
    assert!(matches!(&second[second.len() - 2], Message::Tool { .. }));
    assert_eq!(
        user_messages(second).last().unwrap(),
        "use the v2 API instead"
    );
    // The queued message is a turn of its own, after the first finished.
    assert_eq!(
        user_messages(&requests[2].messages).last().unwrap(),
        "also update the README"
    );
    assert!(position(&ui, "● run_tests {}") < position(&ui, "› use the v2 API instead"));
    assert!(position(&ui, "› use the v2 API instead") < position(&ui, "Switched to the v2 API."));
    assert!(position(&ui, "Switched to the v2 API.") < position(&ui, "› also update the README"));
    assert!(position(&ui, "› also update the README") < position(&ui, "README updated."));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn send_now_input_after_the_last_tool_result_starts_the_next_turn() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(Notify::new());
    let provider = Arc::new(Gated {
        inner: MockProvider::new(vec![
            Script::text("No tools needed."),
            Script::text("Next turn."),
        ]),
        gate: gate.clone(),
        first: true.into(),
    });
    let mut ui = start_with(provider.clone(), dir.path(), &gate);
    type_text(&mut ui, "hello");
    enter(&mut ui);
    tokio::time::timeout(
        Duration::from_secs(10),
        ui.until(|app| app.transcript.busy()),
    )
    .await
    .unwrap()
    .unwrap();
    // This turn runs no tool, so no tool result will take it.
    type_text(&mut ui, "and then this");
    ctrl_s(&mut ui);
    gate.notify_one();
    settle(&mut ui).await;
    let requests = provider.inner.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        user_messages(&requests[1].messages).last().unwrap(),
        "and then this"
    );
    assert!(position(&ui, "No tools needed.") < position(&ui, "› and then this"));
    assert!(position(&ui, "› and then this") < position(&ui, "Next turn."));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn interrupting_puts_waiting_input_back_in_the_editor() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(Notify::new());
    let provider = MockProvider::new(vec![
        Script::tool_call("t1", "run_tests", json!({})),
        Script::text("Resumed."),
    ]);
    let mut ui = start(provider.clone(), dir.path(), &gate);
    type_text(&mut ui, "go");
    enter(&mut ui);
    while_the_tests_run(&mut ui).await;
    type_text(&mut ui, "send this now");
    ctrl_s(&mut ui);
    type_text(&mut ui, "and this later");
    enter(&mut ui);
    type_text(&mut ui, "still typing");
    ui.handle(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
        .unwrap();
    settle(&mut ui).await;
    assert_eq!(
        ui.app().editor().text(),
        "send this now\n\nand this later\n\nstill typing"
    );
    assert_eq!(provider.requests().len(), 1);
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn a_command_cannot_be_sent_during_a_turn_but_can_be_queued() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(Notify::new());
    let provider = MockProvider::new(vec![
        Script::tool_call("t1", "run_tests", json!({})),
        Script::text("Done."),
    ]);
    let mut ui = start(provider.clone(), dir.path(), &gate);
    type_text(&mut ui, "go");
    enter(&mut ui);
    while_the_tests_run(&mut ui).await;
    type_text(&mut ui, "/usage");
    ctrl_s(&mut ui);
    assert_eq!(ui.app().editor().text(), "/usage");
    assert!(
        everything(&ui)
            .iter()
            .any(|r| r == "a command cannot be sent during a turn: press Enter to queue it")
    );
    // Built-in commands that need no turn run at once, even now.
    enter(&mut ui);
    assert!(
        everything(&ui)
            .iter()
            .any(|r| r.starts_with("No tokens used yet"))
    );
    gate.notify_one();
    settle(&mut ui).await;
    ui.finish().await.unwrap();
}
```

Enter during a turn now queues the input, and Esc puts it back in the editor. In `crates/harness-tui/tests/session.rs`:

Replace:

```rust
    while !shows(&ui, "partial answer") {
        ui.next().await.unwrap();
    }
    // Enter while a turn runs keeps the input and says how to stop the turn.
    type_text(&mut ui, "later");
    press(&mut ui, KeyCode::Enter);
    assert_eq!(ui.app().editor().text(), "later");
    assert!(shows(&ui, "a turn is running: press Esc to interrupt it"));
    press(&mut ui, KeyCode::Esc);
    settle(&mut ui).await;
    assert!(shows(&ui, "interrupted"));
    assert!(!ui.app().busy());
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    assert!(shows(&ui, "second answer"));
```

with:

```rust
    while !shows(&ui, "partial answer") {
        ui.next().await.unwrap();
    }
    // Enter while a turn runs queues the input; the interruption puts it back in the editor.
    type_text(&mut ui, "later");
    press(&mut ui, KeyCode::Enter);
    assert_eq!(ui.app().editor().text(), "");
    assert!(shows(&ui, "queued: later"));
    press(&mut ui, KeyCode::Esc);
    settle(&mut ui).await;
    assert!(shows(&ui, "interrupted"));
    assert!(!ui.app().busy());
    assert_eq!(ui.app().editor().text(), "later");
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    assert!(shows(&ui, "second answer"));
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p harness-core --test steering`
Expected: FAIL to compile: ``unresolved import `harness_core::turn::Steering` ``, ``no method named `with_steering` found for struct `Agent` `` and ``no variant named `Steered` found for enum `AgentEvent` ``.

Run: `cargo test -p harness-tui --test session`
Expected: FAIL: `esc_interrupts_a_running_turn_and_the_session_goes_on`, `left: "later"`, `right: ""` (Enter kept the input in the editor).

Run: `cargo test -p harness-tui --test steering`
Expected: FAIL: all four tests; for example `interrupting_puts_waiting_input_back_in_the_editor` with `left: "send this nowand this laterstill typing"` (Ctrl+S did nothing, and Enter kept the input).

- [ ] **Step 3: Steering in the runtime**

In `crates/harness-core/src/turn.rs`:

Replace (1 of 2):

```rust
//! What a user turn sends: text, shell commands whose output is filled in before the message is
//! sent, and settings that apply to that turn only (a slash command's model and allowed tools).

use std::sync::Arc;

use crate::{engine::RuleSet, provider::Provider};

```

with:

```rust
//! What a user turn sends: text, shell commands whose output is filled in before the message is
//! sent, and settings that apply to that turn only (a slash command's model and allowed tools).

use std::sync::{Arc, Mutex};

use crate::{engine::RuleSet, provider::Provider};

```

Replace (2 of 2):

```rust
        TurnInput::from(text.to_string())
    }
}
```

with:

```rust
        TurnInput::from(text.to_string())
    }
}

/// Input the user sends while a turn runs, for the model to get at the next tool-result
/// boundary of that turn ("send now"). The frontend keeps a clone and sends; the agent takes.
#[derive(Debug, Clone, Default)]
pub struct Steering(Arc<Mutex<Vec<String>>>);

impl Steering {
    pub fn new() -> Steering {
        Steering::default()
    }

    /// Adds `text` for the model to get with the next tool results.
    pub fn send(&self, text: impl Into<String>) {
        self.0.lock().expect("steering lock").push(text.into());
    }

    /// Takes everything sent and not yet delivered, oldest first.
    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().expect("steering lock"))
    }

    pub fn is_empty(&self) -> bool {
        self.0.lock().expect("steering lock").is_empty()
    }
}
```

In `crates/harness-core/src/event.rs`:

Replace:

```rust
        reason: String,
        delay_ms: u64,
    },
    /// Something the user should know that did not stop the turn.
    Warning {
        message: String,
```

with:

```rust
        reason: String,
        delay_ms: u64,
    },
    /// Input the user sent while the turn ran, given to the model with the tool results just
    /// sent.
    Steered {
        text: String,
    },
    /// Something the user should know that did not stop the turn.
    Warning {
        message: String,
```

In `crates/harness-core/src/agent.rs`:

Replace (1 of 5):

```rust
    session::{Entry, EntryKind, RewindScope, Session},
    tokens::DEFAULT_CONTEXT_WINDOW,
    tool::{CommandSandbox, Tool, ToolContext, ToolOutput, ToolRegistry},
    turn::{InputPart, TurnInput, TurnModel},
};

/// Model calls allowed per turn unless configured otherwise.
```

with:

```rust
    session::{Entry, EntryKind, RewindScope, Session},
    tokens::DEFAULT_CONTEXT_WINDOW,
    tool::{CommandSandbox, Tool, ToolContext, ToolOutput, ToolRegistry},
    turn::{InputPart, Steering, TurnInput, TurnModel},
};

/// Model calls allowed per turn unless configured otherwise.
```

Replace (2 of 5):

```rust
    stats: Stats,
    /// The sandbox for each mode, when switching modes also switches the sandbox.
    sandboxes: Option<Sandboxes>,
}

impl Agent {
```

with:

```rust
    stats: Stats,
    /// The sandbox for each mode, when switching modes also switches the sandbox.
    sandboxes: Option<Sandboxes>,
    /// Input the user sends while a turn runs.
    steering: Option<Steering>,
}

impl Agent {
```

Replace (3 of 5):

```rust
            auto_compaction_paused: false,
            stats: Stats::default(),
            sandboxes: None,
        }
    }

```

with:

```rust
            auto_compaction_paused: false,
            stats: Stats::default(),
            sandboxes: None,
            steering: None,
        }
    }

```

Replace (4 of 5):

```rust
    /// started with.
    pub fn with_sandboxes(mut self, sandboxes: Sandboxes) -> Self {
        self.sandboxes = Some(sandboxes);
        self
    }

```

with:

```rust
    /// started with.
    pub fn with_sandboxes(mut self, sandboxes: Sandboxes) -> Self {
        self.sandboxes = Some(sandboxes);
        self
    }

    /// Gives the model what the user sends through `steering` while a turn runs, with the
    /// results of the next tool calls.
    pub fn with_steering(mut self, steering: Steering) -> Self {
        self.steering = Some(steering);
        self
    }

```

Replace (5 of 5):

```rust
            if cancel.is_cancelled() {
                return self.finish(TurnEndReason::Interrupted, events);
            }
        }
        self.finish(TurnEndReason::StepLimit, events)
    }

    /// The turn's user message: text parts as they are, and each shell part replaced by the output
```

with:

```rust
            if cancel.is_cancelled() {
                return self.finish(TurnEndReason::Interrupted, events);
            }
            self.deliver_steering(events);
        }
        self.finish(TurnEndReason::StepLimit, events)
    }

    /// Adds what the user sent during the turn to the conversation, after the tool results.
    fn deliver_steering(&mut self, events: &UnboundedSender<AgentEvent>) {
        let Some(steering) = &self.steering else {
            return;
        };
        for text in steering.take() {
            self.record(
                Message::User {
                    content: text.clone(),
                },
                None,
                false,
            );
            let _ = events.send(AgentEvent::Steered { text });
        }
    }

    /// The turn's user message: text parts as they are, and each shell part replaced by the output
```

Run: `cargo test -p harness-core`
Expected: PASS, including `input_sent_now_goes_with_the_next_tool_result_in_the_same_turn`.

- [ ] **Step 4: Queue and send now in the UI**

In `crates/harness-tui/src/app.rs`:

Replace (1 of 10):

```rust
//! Keys and events go in; lines for the scrollback, the live region, and actions for the
//! session to carry out come out.

use std::time::{Duration, Instant};

use harness_context::commands::{is_builtin, parse_invocation};
use harness_core::{
    agent::{ApprovalDecision, ApprovalRequest, ContextUsage},
    event::AgentEvent,
    permission::Mode,
    turn::TurnInput,
};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
```

with:

```rust
//! Keys and events go in; lines for the scrollback, the live region, and actions for the
//! session to carry out come out.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use harness_context::commands::{is_builtin, parse_invocation};
use harness_core::{
    agent::{ApprovalDecision, ApprovalRequest, ContextUsage},
    event::{AgentEvent, TurnEndReason},
    permission::Mode,
    turn::{Steering, TurnInput},
};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
```

Replace (2 of 10):

```rust
    prompt: Option<Prompt>,
    /// The mode chosen while a turn runs, to switch to when it ends.
    pending_mode: Option<Mode>,
    workspace: std::path::PathBuf,
    width: usize,
}
```

with:

```rust
    prompt: Option<Prompt>,
    /// The mode chosen while a turn runs, to switch to when it ends.
    pending_mode: Option<Mode>,
    /// Input to send when the running turn ends: as shown, and in full.
    queued: VecDeque<(String, String)>,
    /// Where send-now input goes; the agent takes it at the next tool result.
    steering: Steering,
    /// Send-now input the agent has not taken yet.
    sent_now: Vec<String>,
    workspace: std::path::PathBuf,
    width: usize,
}
```

Replace (3 of 10):

```rust
            window_note: options.window_note,
            prompt: None,
            pending_mode: None,
            workspace: options.workspace,
            width,
        }
```

with:

```rust
            window_note: options.window_note,
            prompt: None,
            pending_mode: None,
            queued: VecDeque::new(),
            steering: Steering::new(),
            sent_now: Vec::new(),
            workspace: options.workspace,
            width,
        }
```

Replace (4 of 10):

```rust
        self.running
    }

    /// Takes in an event from the agent.
    pub fn on_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::TurnFinished { .. } => self.running = false,
            AgentEvent::Usage { model, usage } => self.totals.add(model, usage),
            _ => {}
        }
        self.transcript.on_event(event, self.width);
    }

    /// Where the next request's tokens go, now.
```

with:

```rust
        self.running
    }

    /// Where send-now input goes: give it to the agent (`Agent::with_steering`).
    pub fn steering(&self) -> Steering {
        self.steering.clone()
    }

    /// Takes in an event from the agent.
    pub fn on_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::TurnFinished { reason } => self.turn_ended(*reason),
            AgentEvent::Usage { model, usage } => self.totals.add(model, usage),
            AgentEvent::Steered { text } => {
                if let Some(i) = self.sent_now.iter().position(|t| t == text) {
                    self.sent_now.remove(i);
                }
            }
            _ => {}
        }
        self.transcript.on_event(event, self.width);
    }

    /// A turn ended. Send-now input it did not take is sent next, before queued input. After an
    /// interruption, both go back into the editor instead, for the user to look at again.
    fn turn_ended(&mut self, reason: TurnEndReason) {
        self.running = false;
        let left = self.steering.take();
        self.sent_now.clear();
        if reason == TurnEndReason::Interrupted {
            let mut parts = left;
            parts.extend(self.queued.drain(..).map(|(_, full)| full));
            if !parts.is_empty() {
                if !self.editor.is_empty() {
                    parts.push(self.editor.expanded());
                }
                self.editor.set_text(&parts.join("\n\n"));
            }
        } else {
            for text in left.into_iter().rev() {
                self.queued.push_front((text.clone(), text));
            }
        }
    }

    /// The next queued input, once no turn runs.
    pub fn next_queued(&mut self) -> Option<Action> {
        if self.busy() || self.prompt.is_some() {
            return None;
        }
        let (shown, full) = self.queued.pop_front()?;
        self.send(shown, full)
    }

    /// Where the next request's tokens go, now.
```

Replace (5 of 10):

```rust
        }
        if ctrl && key.code == KeyCode::Char('d') && self.editor.is_empty() {
            return Some(Action::Quit);
        }
        if let Some(action) = self.completion_key(key) {
            return action;
```

with:

```rust
        }
        if ctrl && key.code == KeyCode::Char('d') && self.editor.is_empty() {
            return Some(Action::Quit);
        }
        if ctrl && key.code == KeyCode::Char('s') {
            return self.send_now();
        }
        if let Some(action) = self.completion_key(key) {
            return action;
```

Replace (6 of 10):

```rust
        self.update_completion();
    }

    /// Enter: sends the input, runs a built-in command, or says why it cannot.
    fn submit(&mut self) -> Option<Action> {
        if self.editor.expanded().trim().is_empty() {
            return None;
        }
        if self.busy() {
            self.hint = Some("a turn is running: press Esc to interrupt it".into());
            return None;
        }
        self.completion = None;
        let full = self.editor.expanded();
        let width = self.width;
        if let Some(invocation) = parse_invocation(&full) {
            let name = invocation.name.to_string();
            if name == "help" {
                self.editor.submit();
                self.transcript.push_user(&full, width);
                self.help();
                return None;
            }
            if name == "quit" {
                return Some(Action::Quit);
            }
            if name == "context" || name == "usage" {
                self.editor.submit();
                self.transcript.push_user(&full, width);
                let theme = self.theme();
                let lines = if name == "context" {
                    status::context_report(
```

with:

```rust
        self.update_completion();
    }

    /// Enter: runs a built-in command now, and sends the input, or queues it while a turn runs.
    fn submit(&mut self) -> Option<Action> {
        let full = self.editor.expanded();
        if full.trim().is_empty() {
            return None;
        }
        self.completion = None;
        if let Some(invocation) = parse_invocation(&full)
            && !self.starts_a_turn(invocation.name)
        {
            let name = invocation.name.to_string();
            return self.builtin(&name, &full);
        }
        let (shown, full) = self.editor.submit();
        if self.busy() {
            self.queued.push_back((shown, full));
            return None;
        }
        self.send(shown, full)
    }

    /// Ctrl+S: while a turn runs, gives the input to the model with the next tool results.
    fn send_now(&mut self) -> Option<Action> {
        if !self.busy() {
            return self.submit();
        }
        let full = self.editor.expanded();
        if full.trim().is_empty() {
            return None;
        }
        if parse_invocation(&full).is_some() {
            self.hint =
                Some("a command cannot be sent during a turn: press Enter to queue it".into());
            return None;
        }
        let (_, full) = self.editor.submit();
        self.steering.send(full.clone());
        self.sent_now.push(full);
        None
    }

    /// Whether `/name` starts a turn: `/init` and custom commands.
    fn starts_a_turn(&self, name: &str) -> bool {
        name == "init" || (!is_builtin(name) && self.host.is_command(name))
    }

    /// A built-in command that runs here, without a turn, or an unknown one.
    fn builtin(&mut self, name: &str, full: &str) -> Option<Action> {
        let width = self.width;
        match name {
            "quit" => return Some(Action::Quit),
            "help" => {
                self.editor.submit();
                self.transcript.push_user(full, width);
                self.help();
            }
            "context" | "usage" => {
                self.editor.submit();
                self.transcript.push_user(full, width);
                let theme = self.theme();
                let lines = if name == "context" {
                    status::context_report(
```

Replace (7 of 10):

```rust
                    self.totals.report(&theme)
                };
                self.transcript.push_lines(lines, width);
                return None;
            }
            if let Some((_, when)) = LATER.iter().find(|(n, _)| *n == name) {
                self.editor.submit();
                self.transcript.push_user(&full, width);
                self.transcript.push_note(
                    &format!("/{name} is not available yet: it comes {when}."),
                    width,
                );
                return None;
            }
            if name == "init" || (!is_builtin(&name) && self.host.is_command(&name)) {
                self.editor.submit();
                let prepared = self.host.prepare(&full);
                self.transcript.push_user(&full, width);
                for warning in &prepared.warnings {
                    self.transcript.push_warning(warning, width);
                }
                for note in &prepared.notes {
                    self.transcript.push_note(note, width);
                }
                return self.run(prepared.input);
            }
            if !is_builtin(&name) {
                self.transcript.push_error(
                    &format!(
                        "unknown command /{name}; custom commands are Markdown files in .harness/commands, .claude/commands or .opencode/commands; to send text that starts with /, put a word before it"
                    ),
                    width,
                );
                return None;
            }
        }
        let (shown, full) = self.editor.submit();
        self.transcript.push_user(&shown, width);
        self.run(TurnInput::from(full))
    }
```

with:

```rust
                    self.totals.report(&theme)
                };
                self.transcript.push_lines(lines, width);
            }
            _ => {
                if let Some((_, when)) = LATER.iter().find(|(n, _)| *n == name) {
                    self.editor.submit();
                    self.transcript.push_user(full, width);
                    self.transcript.push_note(
                        &format!("/{name} is not available yet: it comes {when}."),
                        width,
                    );
                } else {
                    self.transcript.push_error(
                        &format!(
                            "unknown command /{name}; custom commands are Markdown files in .harness/commands, .claude/commands or .opencode/commands; to send text that starts with /, put a word before it"
                        ),
                        width,
                    );
                }
            }
        }
        None
    }

    /// Sends input typed as `shown`, `full` with pastes expanded: a custom command or `/init`
    /// expanded, anything else as it is.
    fn send(&mut self, shown: String, full: String) -> Option<Action> {
        let width = self.width;
        if let Some(invocation) = parse_invocation(&full)
            && self.starts_a_turn(invocation.name)
        {
            let prepared = self.host.prepare(&full);
            self.transcript.push_user(&full, width);
            for warning in &prepared.warnings {
                self.transcript.push_warning(warning, width);
            }
            for note in &prepared.notes {
                self.transcript.push_note(note, width);
            }
            return self.run(prepared.input);
        }
        self.transcript.push_user(&shown, width);
        self.run(TurnInput::from(full))
    }
```

Replace (8 of 10):

```rust
            ("Up, Down", "earlier inputs"),
            ("Tab", "complete a /command or @file"),
            ("Ctrl+O", "expand a collapsed paste"),
            ("Esc", "interrupt the running turn"),
            ("Ctrl+C twice", "exit"),
        ] {
```

with:

```rust
            ("Up, Down", "earlier inputs"),
            ("Tab", "complete a /command or @file"),
            ("Ctrl+O", "expand a collapsed paste"),
            ("Enter during a turn", "send the input when the turn ends"),
            ("Ctrl+S during a turn", "send it with the next tool results"),
            ("Shift+Tab", "switch between plan, ask and auto mode"),
            ("Esc", "interrupt the running turn"),
            ("Ctrl+C twice", "exit"),
        ] {
```

Replace (9 of 10):

```rust
            return (lines.split_off(skip), None);
        }
        below.clear();
        let (editor, cursor) = self.editor.render("› ", width, &theme);
        if let Some(completion) = &self.completion {
            below.extend(complete::render(
                &completion.offer,
```

with:

```rust
            return (lines.split_off(skip), None);
        }
        below.clear();
        let (mut editor, mut cursor) = self.editor.render("› ", width, &theme);
        // What waits to be sent, above the input.
        let mut waiting: Vec<Line<'static>> = Vec::new();
        for text in &self.sent_now {
            waiting.push(pending_line(
                "sending with the next tool results: ",
                text,
                &theme,
            ));
        }
        for (shown, _) in &self.queued {
            waiting.push(pending_line("queued: ", shown, &theme));
        }
        cursor.y += waiting.len() as u16;
        waiting.append(&mut editor);
        let editor = waiting;
        if let Some(completion) = &self.completion {
            below.extend(complete::render(
                &completion.offer,
```

Replace (10 of 10):

```rust
        Mode::Auto | Mode::FullAccess => Mode::Plan,
    }
}
```

with:

```rust
        Mode::Auto | Mode::FullAccess => Mode::Plan,
    }
}

/// One line for input waiting to be sent: `label` and the input's first line.
fn pending_line(label: &str, text: &str, theme: &Theme) -> Line<'static> {
    let first = sanitize(text.lines().next().unwrap_or_default());
    let more = if text.lines().count() > 1 { " …" } else { "" };
    Line::from(vec![
        Span::styled(label.to_string(), theme.dim()),
        Span::raw(format!("{first}{more}")),
    ])
}
```

In `crates/harness-tui/src/transcript.rs`:

Replace:

```rust
                self.push_note(&text, width);
            }
            AgentEvent::Warning { message } => self.push_warning(message, width),
            AgentEvent::Error { message, .. } => self.push_error(message, width),
            AgentEvent::Compacted {
                summary,
```

with:

```rust
                self.push_note(&text, width);
            }
            AgentEvent::Warning { message } => self.push_warning(message, width),
            AgentEvent::Steered { text } => self.push_user(text, width),
            AgentEvent::Error { message, .. } => self.push_error(message, width),
            AgentEvent::Compacted {
                summary,
```

In `crates/harness-tui/src/ui.rs`:

Replace (1 of 2):

```rust
        let (contexts_tx, contexts) = mpsc::unbounded_channel();
        let width = term.width() as usize;
        let mut app = App::new(options, host, width);
        app.set_context(agent.context_usage());
        let runner = tokio::spawn(async move {
            let mut agent = agent;
```

with:

```rust
        let (contexts_tx, contexts) = mpsc::unbounded_channel();
        let width = term.width() as usize;
        let mut app = App::new(options, host, width);
        let agent = agent.with_steering(app.steering());
        app.set_context(agent.context_usage());
        let runner = tokio::spawn(async move {
            let mut agent = agent;
```

Replace (2 of 2):

```rust
        if let Some(action) = self.app.take_pending_mode() {
            self.dispatch(action);
        }
        self.draw()?;
        Ok(Flow::Continue)
    }
```

with:

```rust
        if let Some(action) = self.app.take_pending_mode() {
            self.dispatch(action);
        }
        if let Some(action) = self.app.next_queued() {
            self.dispatch(action);
        }
        self.draw()?;
        Ok(Flow::Continue)
    }
```

- [ ] **Step 5: Run the tests, and lint**

Run: `cargo test -p harness-core -p harness-tui -p harness-cli`
Expected: PASS.

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/harness-core crates/harness-tui
git commit -F - <<'EOF'
feat(tui): queue input during a turn, or send it now with Ctrl+S

Enter while a turn runs queues the input for a new turn when this one
ends; Ctrl+S gives it to the model with the next tool results of the
running turn, through the agent's new Steering handle and a steered
event. Send-now input the turn never took starts the next turn, before
queued input; Esc puts both back into the editor. Built-in commands
that need no turn, such as /usage, still run at once.

<trailer lines from the controller>
EOF
```

---

### Task 11: Plan mode's ending: Build, Edit, Keep planning

**Files:**
- Modify: `crates/harness-core/src/{session,turn,agent}.rs`, `crates/harness-tui/src/{app,ui,lib}.rs`, `crates/harness-cli/src/{interactive,start}.rs`
- Create: `crates/harness-tui/src/plan.rs`
- Test: `crates/harness-core/tests/plan.rs`, `crates/harness-tui/tests/plan.rs`; the sessions built in `crates/harness-core/tests/{session,agent_session}.rs`, `start.rs`'s tests and the other `harness-tui` tests

**Interfaces:**
- Consumes: Task 9's `set_mode` and mode switching; Task 6's `App`, `Ui`; Task 3's `markdown::render`.
- Produces:
  - `TurnInput::plan: Option<String>`, saved as `EntryKind::Message { …, plan: Option<String> }` (serialized only when set), and `Agent::approved_plan() -> Option<String>`; the `plan` mode note asks for a step-by-step implementation plan;
  - `plan::TextEditor` (`edit(&str) -> io::Result<String>`), `plan::ExternalEditor<W, R>` (`from_env(modes)`, `new(command, modes)`), `plan::PlanChoice { plan, edited }` (`key`, `render`, `build_message`), `plan::Choice { Build, Edit, KeepPlanning }`;
  - `Options { …, default_mode: Mode, text_editor: Option<Box<dyn TextEditor>> }`, `Action::RunIn(Mode, TurnInput)`, `Action::EditPlan(String)`, `App::plan_choice()`, `App::plan_edited(io::Result<String>)`.

- [ ] **Step 1: Write the failing tests**

Create `crates/harness-core/tests/plan.rs`:

```rust
//! Plan mode: the note that asks for a plan, and the approved plan saved in the session.

mod common;

use std::sync::Arc;

use common::{Echo, agent, run};
use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    engine::{EngineConfig, PermissionEngine},
    message::Message,
    permission::Mode,
    session::Session,
    testing::{MockProvider, Script},
    tool::{ToolContext, ToolRegistry},
    turn::{InputPart, TurnInput},
};

fn notes(provider: &MockProvider) -> String {
    provider
        .requests()
        .last()
        .unwrap()
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::User { content } => Some(content.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn plan_mode_asks_for_a_step_by_step_plan_in_the_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::text("1. read\n2. change")]);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    agent.set_mode(Mode::Plan);
    run(&mut agent, "add a login rate limiter").await;
    let sent = notes(&provider);
    assert!(
        sent.contains("[harness] The approval mode is now plan"),
        "{sent}"
    );
    assert!(
        sent.contains("end your reply with a step-by-step implementation plan"),
        "{sent}"
    );
    // The system prompt is left alone.
    assert_eq!(provider.requests()[0].system, "system prompt");
    // Read-only mode is not plan mode: no plan is asked for.
    agent.set_mode(Mode::ReadOnly);
    let history = agent.history();
    let Message::User { content } = history.last().unwrap() else {
        panic!("a note");
    };
    assert!(!content.contains("plan"), "{content}");
}

fn saved_agent(provider: Arc<MockProvider>, session: Session, dir: &std::path::Path) -> Agent {
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode: Mode::Auto,
        workspace: dir.to_path_buf(),
        read_dirs: vec![],
        rules: Default::default(),
        sandbox_available: false,
        writes_need_approval: false,
    }));
    Agent::new(
        provider,
        ToolRegistry::new(vec![Arc::new(Echo)]),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m1", "m1", "system prompt", dir.join(".spill")),
        ToolContext::new(dir),
    )
    .with_session(session)
}

#[tokio::test]
async fn the_approved_plan_is_saved_with_the_turn_that_builds_it() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let provider = MockProvider::new(vec![Script::text("Done.")]);
    let mut agent = saved_agent(
        provider.clone(),
        Session::create(&sessions, dir.path()),
        dir.path(),
    );
    assert_eq!(agent.approved_plan(), None);
    let plan = "1. Add a limiter\n2. Test it".to_string();
    let input = TurnInput {
        parts: vec![InputPart::Text("Implement the plan above.".into())],
        display: Some("Build the plan".into()),
        plan: Some(plan.clone()),
        ..TurnInput::default()
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_turn(input, &tx, tokio_util::sync::CancellationToken::new())
        .await;
    assert_eq!(agent.approved_plan(), Some(plan.clone()));
    let path = agent.session().path().unwrap().to_path_buf();
    drop(agent);
    // It is in the file, and read back on resume.
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("\"plan\":\"1. Add a limiter\\n2. Test it\""),
        "{text}"
    );
    let (session, _) = Session::open(&path).unwrap();
    let agent = saved_agent(MockProvider::new(vec![]), session, dir.path());
    assert_eq!(agent.approved_plan(), Some(plan));
    assert_eq!(agent.rewind_points().last().unwrap().text, "Build the plan");
}
```

Message entries built by tests gain the field. In `crates/harness-core/tests/session.rs`:

Replace (1 of 3):

```rust
        message: Message::User {
            content: text.into(),
        },
        display: None,
        note: false,
    }
}

fn assistant(text: &str) -> EntryKind {
    EntryKind::Message {
```

with:

```rust
        message: Message::User {
            content: text.into(),
        },
        display: None,
        note: false,
        plan: None,
    }
}

fn assistant(text: &str) -> EntryKind {
    EntryKind::Message {
```

Replace (2 of 3):

```rust
            tool_calls: vec![],
            model: "mock/m".into(),
        },
        display: None,
        note: false,
    }
}

fn texts(session: &Session) -> Vec<String> {
    session
```

with:

```rust
            tool_calls: vec![],
            model: "mock/m".into(),
        },
        display: None,
        note: false,
        plan: None,
    }
}

fn texts(session: &Session) -> Vec<String> {
    session
```

Replace (3 of 3):

```rust
        message: Message::User {
            content: "the expanded command".into(),
        },
        display: Some("/opsx:propose add-login".into()),
        note: false,
    });
    std::thread::sleep(std::time::Duration::from_millis(20));
    let mut newer = Session::create(dir.path(), Path::new("/work"));
    newer.append(EntryKind::Message {
        message: Message::User {
            content: "[harness] note".into(),
        },
        display: None,
        note: true,
    });
    newer.append(user("fix the tests"));
    let listed = session::list(dir.path());
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, newer.id());
```

with:

```rust
        message: Message::User {
            content: "the expanded command".into(),
        },
        display: Some("/opsx:propose add-login".into()),
        note: false,
        plan: None,
    });
    std::thread::sleep(std::time::Duration::from_millis(20));
    let mut newer = Session::create(dir.path(), Path::new("/work"));
    newer.append(EntryKind::Message {
        message: Message::User {
            content: "[harness] note".into(),
        },
        display: None,
        note: true,
        plan: None,
    });
    newer.append(user("fix the tests"));
    let listed = session::list(dir.path());
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, newer.id());
```

In `crates/harness-core/tests/agent_session.rs`:

Replace:

```rust
        message,
        display: None,
        note: false,
    }
}

```

with:

```rust
        message,
        display: None,
        note: false,
        plan: None,
    }
}

```

In `crates/harness-cli/src/start.rs`:

Replace:

```rust
            },
            display: None,
            note: false,
        });
        let path = session.path().unwrap().to_path_buf();
        let policy = Arc::new(PermissionEngine::new(EngineConfig {
```

with:

```rust
            },
            display: None,
            note: false,
            plan: None,
        });
        let path = session.path().unwrap().to_path_buf();
        let policy = Arc::new(PermissionEngine::new(EngineConfig {
```

Create `crates/harness-tui/tests/plan.rs`:

```rust
//! Plan mode's flow: a planning turn ends with the plan and three choices, Build goes back to
//! the mode before plan mode and implements it, Edit opens it in the user's editor, and Keep
//! planning stays.

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use harness_core::{
    agent::{Agent, AgentConfig},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    message::Message,
    permission::Mode,
    session::Session,
    testing::{MockProvider, Script},
    tool::{Tool, ToolContext, ToolRegistry},
};
use harness_tui::{
    app::{Host, Options, Prepared},
    approval::ChannelApprover,
    inline::InlineTerminal,
    plan::{ExternalEditor, TextEditor},
    style::Theme,
    terminal::{Modes, RawMode},
    ui::Ui,
};
use ratatui::{
    backend::TestBackend,
    buffer::Buffer,
    crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers},
};
use serde_json::json;

const PLAN: &str = "1. Read src/login.rs\n2. Add a limiter\n3. Test it";

struct NoCommands;

impl Host for NoCommands {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
}

/// Deletes step 3, as a user would in their editor.
struct DeleteStepThree;

impl TextEditor for DeleteStepThree {
    fn edit(&mut self, text: &str) -> std::io::Result<String> {
        Ok(text
            .lines()
            .filter(|l| !l.starts_with("3."))
            .map(|l| format!("{l}\n"))
            .collect())
    }
}

fn start(
    provider: Arc<MockProvider>,
    dir: &Path,
    mode: Mode,
    default_mode: Mode,
) -> Ui<TestBackend> {
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode,
        workspace: dir.to_path_buf(),
        read_dirs: Vec::new(),
        rules: RuleSet::default(),
        sandbox_available: true,
        writes_need_approval: false,
    }));
    let tools: Vec<Arc<dyn Tool>> = ["read", "write"]
        .iter()
        .map(|name| harness_tools::builtin().get(name).unwrap())
        .collect();
    let (approver, approvals) = ChannelApprover::new();
    let agent = Agent::new(
        provider,
        ToolRegistry::new(tools),
        policy,
        approver,
        AgentConfig::new("mock/m", "m", "system", dir.join("out")),
        ToolContext::new(dir).with_sandbox(None, mode.fs_access()),
    )
    .with_session(Session::create(&dir.join("sessions"), dir));
    let options = Options {
        theme: Theme::monochrome(),
        model: "mock/m".into(),
        mode,
        commands: Vec::new(),
        workspace: dir.to_path_buf(),
        history: Vec::new(),
        instruction_files: Vec::new(),
        window_note: None,
        default_mode,
        text_editor: Some(Box::new(DeleteStepThree)),
    };
    let term = InlineTerminal::new(TestBackend::new(100, 30), 0).unwrap();
    let mut ui = Ui::start(agent, Box::new(NoCommands), term, options, approvals);
    ui.draw().unwrap();
    ui
}

fn rows(buffer: &Buffer) -> Vec<String> {
    let width = buffer.area.width as usize;
    buffer
        .content
        .chunks(width.max(1))
        .map(|row| {
            let text: String = row.iter().map(|cell| cell.symbol()).collect();
            text.trim_end().to_string()
        })
        .collect()
}

fn everything(ui: &Ui<TestBackend>) -> Vec<String> {
    let backend = ui.terminal().backend();
    let mut out = rows(backend.scrollback());
    out.extend(rows(backend.buffer()));
    out
}

fn status(ui: &Ui<TestBackend>) -> String {
    rows(ui.terminal().backend().buffer())
        .into_iter()
        .rfind(|r| r.starts_with("mock/m · "))
        .unwrap()
}

fn press(ui: &mut Ui<TestBackend>, code: KeyCode) {
    ui.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        .unwrap();
}

fn send(ui: &mut Ui<TestBackend>, text: &str) {
    for c in text.chars() {
        press(ui, KeyCode::Char(c));
    }
    press(ui, KeyCode::Enter);
}

async fn settle(ui: &mut Ui<TestBackend>) {
    tokio::time::timeout(Duration::from_secs(10), ui.settle())
        .await
        .expect("settles")
        .unwrap();
}

fn last_user(provider: &MockProvider) -> String {
    provider
        .requests()
        .last()
        .unwrap()
        .messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::User { content } => Some(content.clone()),
            _ => None,
        })
        .unwrap()
}

/// Plans in `ui` (in plan mode already), with the model first trying to write a file.
async fn plan(ui: &mut Ui<TestBackend>) {
    send(ui, "add a login rate limiter");
    settle(ui).await;
}

fn planning_script(then: Vec<Script>) -> Arc<MockProvider> {
    let mut script = vec![
        Script::tool_call("w1", "write", json!({"path": "limiter.rs", "content": "x"})),
        Script::text(PLAN),
    ];
    script.extend(then);
    MockProvider::new(script)
}

#[tokio::test]
async fn a_planning_turn_changes_nothing_and_ends_with_the_plan_and_three_choices() {
    let dir = tempfile::tempdir().unwrap();
    let provider = planning_script(vec![]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Auto, Mode::Auto);
    ui.handle(Event::Key(KeyEvent::new(
        KeyCode::BackTab,
        KeyModifiers::SHIFT,
    )))
    .unwrap();
    plan(&mut ui).await;
    assert!(!dir.path().join("limiter.rs").exists());
    let asked = provider.requests()[0]
        .messages
        .iter()
        .map(|m| format!("{m:?}"))
        .collect::<String>();
    assert!(
        asked.contains("step-by-step implementation plan"),
        "{asked}"
    );
    let screen = everything(&ui);
    assert!(screen.iter().any(|r| r == "3. Test it"), "{screen:#?}");
    assert!(
        screen.iter().any(|r| r
            == "The plan is ready: [b] build it  [e] edit it in your editor  [k] keep planning"),
        "{screen:#?}"
    );
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn build_goes_back_to_the_mode_before_plan_and_implements_the_plan() {
    let dir = tempfile::tempdir().unwrap();
    let provider = planning_script(vec![Script::text("Implemented.")]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Auto, Mode::Ask);
    ui.handle(Event::Key(KeyEvent::new(
        KeyCode::BackTab,
        KeyModifiers::SHIFT,
    )))
    .unwrap();
    plan(&mut ui).await;
    press(&mut ui, KeyCode::Char('b'));
    settle(&mut ui).await;
    assert!(
        status(&ui).starts_with("mock/m · auto ·"),
        "{}",
        status(&ui)
    );
    // The mode change and the request to build go as one user message.
    let build = last_user(&provider);
    assert!(
        build.starts_with("[harness] The approval mode is now auto"),
        "{build}"
    );
    assert!(build.ends_with("\n\nImplement the plan above."), "{build}");
    assert!(everything(&ui).iter().any(|r| r == "› Build the plan"));
    assert!(everything(&ui).iter().any(|r| r == "Implemented."));
    ui.finish().await.unwrap();
    // The session keeps the approved plan.
    let sessions = dir.path().join("sessions");
    let file = std::fs::read_dir(&sessions)
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let text = std::fs::read_to_string(file.path()).unwrap();
    assert!(
        text.contains(&format!(
            "\"plan\":{}",
            serde_json::to_string(PLAN).unwrap()
        )),
        "{text}"
    );
}

#[tokio::test]
async fn an_edited_plan_is_shown_again_and_built_as_edited() {
    let dir = tempfile::tempdir().unwrap();
    let provider = planning_script(vec![Script::text("Implemented.")]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Plan, Mode::Auto);
    plan(&mut ui).await;
    press(&mut ui, KeyCode::Char('e'));
    let screen = everything(&ui);
    let edited = screen
        .iter()
        .position(|r| r == "the edited plan:")
        .expect("the edited plan");
    assert_eq!(screen[edited + 1], "1. Read src/login.rs");
    assert_eq!(screen[edited + 2], "2. Add a limiter");
    assert!(
        !screen[edited..].iter().any(|r| r == "3. Test it"),
        "{screen:#?}"
    );
    assert!(
        screen
            .iter()
            .any(|r| r.starts_with("The edited plan is ready: [b] build it"))
    );
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    let build = last_user(&provider);
    assert!(
        build.ends_with("\n\nImplement this plan:\n\n1. Read src/login.rs\n2. Add a limiter\n"),
        "{build}"
    );
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn keep_planning_stays_in_plan_mode_and_returns_to_the_input() {
    let dir = tempfile::tempdir().unwrap();
    let provider = planning_script(vec![Script::text("A better plan.")]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Plan, Mode::Auto);
    plan(&mut ui).await;
    press(&mut ui, KeyCode::Char('k'));
    assert!(ui.app().plan_choice().is_none());
    assert!(
        everything(&ui)
            .iter()
            .any(|r| r == "still planning: say what to change")
    );
    assert!(status(&ui).starts_with("mock/m · plan ·"));
    send(&mut ui, "also cover the API");
    settle(&mut ui).await;
    assert!(ui.app().plan_choice().is_some());
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn a_session_started_in_plan_mode_is_told_to_plan_and_builds_in_the_default_mode() {
    let dir = tempfile::tempdir().unwrap();
    let provider = planning_script(vec![Script::text("Implemented.")]);
    let mut ui = start(provider.clone(), dir.path(), Mode::Plan, Mode::Ask);
    plan(&mut ui).await;
    let first = format!("{:?}", provider.requests()[0].messages);
    assert!(first.contains("The approval mode is now plan"), "{first}");
    press(&mut ui, KeyCode::Char('b'));
    settle(&mut ui).await;
    assert!(status(&ui).starts_with("mock/m · ask ·"), "{}", status(&ui));
    ui.finish().await.unwrap();
}

/// Records whether raw mode is on.
struct FakeRaw(Arc<Mutex<Vec<&'static str>>>);

impl RawMode for FakeRaw {
    fn enable(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().push("raw on");
        Ok(())
    }
    fn disable(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().push("raw off");
        Ok(())
    }
}

#[test]
fn the_editor_edits_the_plan_with_the_terminal_given_back_meanwhile() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut out = Vec::new();
    {
        let modes = Modes::enter(&mut out, FakeRaw(log.clone()), false).unwrap();
        let mut editor = ExternalEditor::new(
            r#"f() { grep -v '^3\.' "$1" > "$1.new"; mv "$1.new" "$1"; }; f"#.into(),
            modes,
        );
        let edited = editor.edit("1. a\n2. b\n3. c\n").unwrap();
        assert_eq!(edited, "1. a\n2. b\n");
        assert_eq!(*log.lock().unwrap(), ["raw on", "raw off", "raw on"]);
        // An editor that fails leaves the plan as it was.
        let mut failing = ExternalEditor::new(
            "false".into(),
            Modes::enter(Vec::new(), FakeRaw(log.clone()), false).unwrap(),
        );
        let error = failing.edit("plan").unwrap_err();
        assert!(error.to_string().contains("exited with"), "{error}");
    }
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "\x1b[?2004h\x1b[?2004l\x1b[?2004h\x1b[?2004l"
    );
    // The plan's file is gone.
    let left = std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(&format!("harness-plan-{}-", std::process::id()))
        })
        .count();
    assert_eq!(left, 0);
}
```

The other sessions' options gain the default mode and the editor. In `crates/harness-tui/tests/session.rs`:

Replace:

```rust
        history: Vec::new(),
        instruction_files: Vec::new(),
        window_note: None,
    }
}

```

with:

```rust
        history: Vec::new(),
        instruction_files: Vec::new(),
        window_note: None,
        default_mode: Mode::Auto,
        text_editor: None,
    }
}

```

In `crates/harness-tui/tests/status.rs`:

Replace:

```rust
        history: Vec::new(),
        instruction_files: vec![("AGENTS.md".into(), 2_000)],
        window_note: Some("assumed".into()),
    };
    let term = InlineTerminal::new(TestBackend::new(70, 20), 0).unwrap();
    let mut ui = Ui::start(
```

with:

```rust
        history: Vec::new(),
        instruction_files: vec![("AGENTS.md".into(), 2_000)],
        window_note: Some("assumed".into()),
        default_mode: Mode::Auto,
        text_editor: None,
    };
    let term = InlineTerminal::new(TestBackend::new(70, 20), 0).unwrap();
    let mut ui = Ui::start(
```

In `crates/harness-tui/tests/approvals.rs`:

Replace:

```rust
        history: Vec::new(),
        instruction_files: Vec::new(),
        window_note: None,
    };
    let term = InlineTerminal::new(TestBackend::new(80, 24), 0).unwrap();
    let mut ui = Ui::start(agent, Box::new(NoCommands), term, options, approvals);
```

with:

```rust
        history: Vec::new(),
        instruction_files: Vec::new(),
        window_note: None,
        default_mode: Mode::Auto,
        text_editor: None,
    };
    let term = InlineTerminal::new(TestBackend::new(80, 24), 0).unwrap();
    let mut ui = Ui::start(agent, Box::new(NoCommands), term, options, approvals);
```

In `crates/harness-tui/tests/modes.rs`:

Replace:

```rust
        history: Vec::new(),
        instruction_files: Vec::new(),
        window_note: None,
    };
    let term = InlineTerminal::new(TestBackend::new(100, 20), 0).unwrap();
    let mut ui = Ui::start(
```

with:

```rust
        history: Vec::new(),
        instruction_files: Vec::new(),
        window_note: None,
        default_mode: Mode::Auto,
        text_editor: None,
    };
    let term = InlineTerminal::new(TestBackend::new(100, 20), 0).unwrap();
    let mut ui = Ui::start(
```

In `crates/harness-tui/tests/steering.rs`:

Replace:

```rust
        history: Vec::new(),
        instruction_files: Vec::new(),
        window_note: None,
    };
    let term = InlineTerminal::new(TestBackend::new(80, 24), 0).unwrap();
    let mut ui = Ui::start(
```

with:

```rust
        history: Vec::new(),
        instruction_files: Vec::new(),
        window_note: None,
        default_mode: Mode::Auto,
        text_editor: None,
    };
    let term = InlineTerminal::new(TestBackend::new(80, 24), 0).unwrap();
    let mut ui = Ui::start(
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p harness-core --test plan --test session`
Expected: FAIL to compile: ``variant `EntryKind::Message` has no field named `plan` ``, ``struct `TurnInput` has no field named `plan` `` and ``no method named `approved_plan` found for struct `Agent` ``.

Run: `cargo test -p harness-tui --test plan`
Expected: FAIL to compile: ``unresolved import `harness_tui::plan` ``.

- [ ] **Step 3: The approved plan in the session, and the planning note**

In `crates/harness-core/src/session.rs`:

Replace (1 of 2):

```rust
        /// A note from harness, such as a mode change, rather than something the user typed.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        note: bool,
    },
    /// A summary that replaces the conversation before `first_kept` (all of it when `None`) on
    /// this branch. The summarized entries stay in the file.
```

with:

```rust
        /// A note from harness, such as a mode change, rather than something the user typed.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        note: bool,
        /// The plan the user approved, on the message that asks the model to build it. A harness
        /// that does not know the field ignores it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan: Option<String>,
    },
    /// A summary that replaces the conversation before `first_kept` (all of it when `None`) on
    /// this branch. The summarized entries stay in the file.
```

Replace (2 of 2):

```rust
                    message: Message::User { content },
                    display,
                    note: false,
                } => Some(display.unwrap_or(content)),
                _ => None,
            }
```

with:

```rust
                    message: Message::User { content },
                    display,
                    note: false,
                    ..
                } => Some(display.unwrap_or(content)),
                _ => None,
            }
```

In `crates/harness-core/src/turn.rs`:

Replace:

```rust
    pub rules: RuleSet,
    /// Run this turn's shell commands in a read-only sandbox.
    pub read_only_shell: bool,
}

impl From<String> for TurnInput {
```

with:

```rust
    pub rules: RuleSet,
    /// Run this turn's shell commands in a read-only sandbox.
    pub read_only_shell: bool,
    /// The plan the user approved, which this turn implements; saved with its user message.
    pub plan: Option<String>,
}

impl From<String> for TurnInput {
```

In `crates/harness-core/src/agent.rs`:

Replace (1 of 5):

```rust
                    message: Message::User { content },
                    display,
                    note: false,
                } => Some(RewindPoint {
                    entry: entry.id.clone(),
                    text: display.clone().unwrap_or_else(|| content.clone()),
```

with:

```rust
                    message: Message::User { content },
                    display,
                    note: false,
                    ..
                } => Some(RewindPoint {
                    entry: entry.id.clone(),
                    text: display.clone().unwrap_or_else(|| content.clone()),
```

Replace (2 of 5):

```rust
                _ => None,
            })
            .collect()
    }

    /// Whether the rewind list offers "undo last rewind": nothing has happened since the last
```

with:

```rust
                _ => None,
            })
            .collect()
    }

    /// The plan the user last approved on the active branch, if any.
    pub fn approved_plan(&self) -> Option<String> {
        self.session
            .branch()
            .into_iter()
            .rev()
            .find_map(|entry| match &entry.kind {
                EntryKind::Message {
                    plan: Some(plan), ..
                } => Some(plan.clone()),
                _ => None,
            })
    }

    /// Whether the rewind list offers "undo last rewind": nothing has happened since the last
```

Replace (3 of 5):

```rust
    /// Adds `message` to the history and saves it in the session. If the session file cannot be
    /// written, the conversation continues in memory and a warning says so once.
    fn record(&mut self, message: Message, display: Option<String>, note: bool) {
        let id = self.session.append(EntryKind::Message {
            message: message.clone(),
            display,
            note,
        });
        self.history.push(message);
        self.history_ids.push(id);
```

with:

```rust
    /// Adds `message` to the history and saves it in the session. If the session file cannot be
    /// written, the conversation continues in memory and a warning says so once.
    fn record(&mut self, message: Message, display: Option<String>, note: bool) {
        self.record_entry(message, display, note, None);
    }

    /// [`record`](Self::record), with the plan the message asks to build.
    fn record_entry(
        &mut self,
        message: Message,
        display: Option<String>,
        note: bool,
        plan: Option<String>,
    ) {
        let id = self.session.append(EntryKind::Message {
            message: message.clone(),
            display,
            note,
            plan,
        });
        self.history.push(message);
        self.history_ids.push(id);
```

Replace (4 of 5):

```rust
        let _ = events.send(AgentEvent::TurnStarted);
        self.message_recorded = false;
        let content = self.user_message(input.parts, events).await;
        self.record(Message::User { content }, input.display, false);
        self.message_recorded = true;
        for kind in std::mem::take(&mut self.held_entries) {
            self.append_turn_entry(kind);
```

with:

```rust
        let _ = events.send(AgentEvent::TurnStarted);
        self.message_recorded = false;
        let content = self.user_message(input.parts, events).await;
        self.record_entry(Message::User { content }, input.display, false, input.plan);
        self.message_recorded = true;
        for kind in std::mem::take(&mut self.held_entries) {
            self.append_turn_entry(kind);
```

Replace (5 of 5):

```rust
/// prompt does.
fn mode_note(mode: Mode, sandboxed: bool) -> String {
    let rules = match mode {
        Mode::Plan | Mode::ReadOnly if sandboxed => {
            "file edits are refused, and shell commands run in a read-only sandbox"
        }
        Mode::Plan | Mode::ReadOnly => {
            "file edits and shell commands are refused, since no OS sandbox is active; use the read, grep and glob tools"
        }
        Mode::Ask if sandboxed => {
```

with:

```rust
/// prompt does.
fn mode_note(mode: Mode, sandboxed: bool) -> String {
    let rules = match mode {
        Mode::Plan if sandboxed => {
            "file edits are refused, and shell commands run in a read-only sandbox. Investigate the task, then end your reply with a step-by-step implementation plan; the user will build it, edit it, or keep planning"
        }
        Mode::Plan => {
            "file edits and shell commands are refused, since no OS sandbox is active; use the read, grep and glob tools. Investigate the task, then end your reply with a step-by-step implementation plan; the user will build it, edit it, or keep planning"
        }
        Mode::ReadOnly if sandboxed => {
            "file edits are refused, and shell commands run in a read-only sandbox"
        }
        Mode::ReadOnly => {
            "file edits and shell commands are refused, since no OS sandbox is active; use the read, grep and glob tools"
        }
        Mode::Ask if sandboxed => {
```

Run: `cargo test -p harness-core`
Expected: PASS, including `plan_mode_asks_for_a_step_by_step_plan_in_the_conversation` and `the_approved_plan_is_saved_with_the_turn_that_builds_it`.

- [ ] **Step 4: The three choices, and the editor**

Create `crates/harness-tui/src/plan.rs`:

```rust
//! Plan mode's ending: once a planning turn ends with a plan, the user builds it, edits it in
//! their editor, or keeps planning.

use std::{
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use ratatui::{
    crossterm::event::{KeyCode, KeyEvent},
    text::{Line, Span},
};

use crate::{
    style::Theme,
    terminal::{Modes, RawMode},
    text::wrap,
};

/// Opens text in the user's editor and returns it as saved.
pub trait TextEditor: Send {
    fn edit(&mut self, text: &str) -> io::Result<String>;
}

/// The user's `$EDITOR` (`vi` without one), run with the terminal's modes undone meanwhile.
pub struct ExternalEditor<W: Write + Send, R: RawMode + Send> {
    /// Run as `sh -c '<command> "$1"'`, so it may hold arguments, as `$EDITOR` often does.
    command: String,
    modes: Modes<W, R>,
}

impl<W: Write + Send, R: RawMode + Send> ExternalEditor<W, R> {
    /// The editor `$EDITOR` names, or `vi`, owning the terminal's `modes` for the session.
    pub fn from_env(modes: Modes<W, R>) -> Self {
        let command = std::env::var("EDITOR")
            .ok()
            .filter(|e| !e.trim().is_empty())
            .unwrap_or_else(|| "vi".into());
        ExternalEditor::new(command, modes)
    }

    pub fn new(command: String, modes: Modes<W, R>) -> Self {
        ExternalEditor { command, modes }
    }
}

/// A new file for the plan, readable only by the user.
fn plan_file() -> io::Result<(PathBuf, std::fs::File)> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    for _ in 0..100 {
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("harness-plan-{}-{n}.md", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("cannot create a file for the plan"))
}

impl<W: Write + Send, R: RawMode + Send> TextEditor for ExternalEditor<W, R> {
    fn edit(&mut self, text: &str) -> io::Result<String> {
        let (path, mut file) = plan_file()?;
        let result = (|| {
            file.write_all(text.as_bytes())?;
            drop(file);
            self.modes.suspend()?;
            let status = Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("{} \"$1\"", self.command))
                .arg("harness")
                .arg(&path)
                .status();
            self.modes.resume()?;
            let status = status?;
            if !status.success() {
                return Err(io::Error::other(format!(
                    "the editor `{}` exited with {status}",
                    self.command
                )));
            }
            std::fs::read_to_string(&path)
        })();
        let _ = std::fs::remove_file(&path);
        result
    }
}

/// What the user chose for a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Build,
    Edit,
    KeepPlanning,
}

/// A plan waiting for the user's choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanChoice {
    pub plan: String,
    /// The user edited it, so the model has not seen it as it is.
    pub edited: bool,
}

impl PlanChoice {
    /// The choice a key makes, if any.
    pub fn key(&self, key: KeyEvent) -> Option<Choice> {
        match key.code {
            KeyCode::Char('b') | KeyCode::Enter => Some(Choice::Build),
            KeyCode::Char('e') => Some(Choice::Edit),
            KeyCode::Char('k') | KeyCode::Esc => Some(Choice::KeepPlanning),
            _ => None,
        }
    }

    /// The prompt, `width` columns wide.
    pub fn render(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let title = if self.edited {
            "The edited plan is ready: "
        } else {
            "The plan is ready: "
        };
        let line = Line::from(vec![
            Span::styled(title, theme.bold()),
            Span::styled("[b] ", theme.accent()),
            Span::raw("build it  "),
            Span::styled("[e] ", theme.accent()),
            Span::raw("edit it in your editor  "),
            Span::styled("[k] ", theme.accent()),
            Span::raw("keep planning"),
        ]);
        wrap(&line, width, &[], &[Span::raw("  ")])
    }

    /// What the build turn sends: the plan above, or the edited plan itself.
    pub fn build_message(&self) -> String {
        if self.edited {
            format!("Implement this plan:\n\n{}", self.plan)
        } else {
            "Implement the plan above.".into()
        }
    }
}
```

In `crates/harness-tui/src/app.rs`:

Replace (1 of 12):

```rust
    approval::{Answered, Prompt, Reply},
    complete::{self, Completer, Offer},
    editor::{Edit, Editor},
    status::{self, Totals},
    style::Theme,
    text::{sanitize, wrap},
```

with:

```rust
    approval::{Answered, Prompt, Reply},
    complete::{self, Completer, Offer},
    editor::{Edit, Editor},
    plan::{Choice, PlanChoice, TextEditor},
    status::{self, Totals},
    style::Theme,
    text::{sanitize, wrap},
```

Replace (2 of 12):

```rust
    pub instruction_files: Vec<(String, u64)>,
    /// Said next to the context window in `/context`, such as where its size comes from.
    pub window_note: Option<String>,
}

/// What the session should do after a key.
```

with:

```rust
    pub instruction_files: Vec<(String, u64)>,
    /// Said next to the context window in `/context`, such as where its size comes from.
    pub window_note: Option<String>,
    /// The mode Build switches to when the session started in plan mode.
    pub default_mode: Mode,
    /// Opens a plan in the user's editor.
    pub text_editor: Option<Box<dyn TextEditor>>,
}

/// What the session should do after a key.
```

Replace (3 of 12):

```rust
    Run(TurnInput),
    /// Switch the approval mode.
    SetMode(Mode),
    /// Stop the running turn.
    Interrupt,
    /// Leave harness.
```

with:

```rust
    Run(TurnInput),
    /// Switch the approval mode.
    SetMode(Mode),
    /// Switch the approval mode, then start a turn.
    RunIn(Mode, TurnInput),
    /// Open the plan in the user's editor.
    EditPlan(String),
    /// Stop the running turn.
    Interrupt,
    /// Leave harness.
```

Replace (4 of 12):

```rust
    steering: Steering,
    /// Send-now input the agent has not taken yet.
    sent_now: Vec<String>,
    workspace: std::path::PathBuf,
    width: usize,
}
```

with:

```rust
    steering: Steering,
    /// Send-now input the agent has not taken yet.
    sent_now: Vec<String>,
    /// The turn's last reply, which in plan mode is the plan.
    last_reply: String,
    /// The mode to go back to when a plan is built.
    mode_before_plan: Option<Mode>,
    default_mode: Mode,
    /// The session started in plan mode, and the agent has not been told to plan yet.
    plan_note_pending: bool,
    /// A plan waiting for the user's choice.
    plan_choice: Option<PlanChoice>,
    workspace: std::path::PathBuf,
    width: usize,
}
```

Replace (5 of 12):

```rust
            queued: VecDeque::new(),
            steering: Steering::new(),
            sent_now: Vec::new(),
            workspace: options.workspace,
            width,
        }
```

with:

```rust
            queued: VecDeque::new(),
            steering: Steering::new(),
            sent_now: Vec::new(),
            last_reply: String::new(),
            mode_before_plan: None,
            default_mode: options.default_mode,
            plan_note_pending: options.mode == Mode::Plan,
            plan_choice: None,
            workspace: options.workspace,
            width,
        }
```

Replace (6 of 12):

```rust
    /// Takes in an event from the agent.
    pub fn on_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::TurnFinished { reason } => self.turn_ended(*reason),
            AgentEvent::Usage { model, usage } => self.totals.add(model, usage),
            AgentEvent::Steered { text } => {
```

with:

```rust
    /// Takes in an event from the agent.
    pub fn on_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::TurnStarted => self.last_reply.clear(),
            AgentEvent::AssistantMessage { content, .. } if !content.trim().is_empty() => {
                self.last_reply = content.clone();
            }
            AgentEvent::TurnFinished { reason } => self.turn_ended(*reason),
            AgentEvent::Usage { model, usage } => self.totals.add(model, usage),
            AgentEvent::Steered { text } => {
```

Replace (7 of 12):

```rust
    /// interruption, both go back into the editor instead, for the user to look at again.
    fn turn_ended(&mut self, reason: TurnEndReason) {
        self.running = false;
        let left = self.steering.take();
        self.sent_now.clear();
        if reason == TurnEndReason::Interrupted {
```

with:

```rust
    /// interruption, both go back into the editor instead, for the user to look at again.
    fn turn_ended(&mut self, reason: TurnEndReason) {
        self.running = false;
        if reason == TurnEndReason::Completed
            && self.mode == Mode::Plan
            && !self.last_reply.trim().is_empty()
        {
            self.plan_choice = Some(PlanChoice {
                plan: self.last_reply.clone(),
                edited: false,
            });
        }
        let left = self.steering.take();
        self.sent_now.clear();
        if reason == TurnEndReason::Interrupted {
```

Replace (8 of 12):

```rust

    /// The next queued input, once no turn runs.
    pub fn next_queued(&mut self) -> Option<Action> {
        if self.busy() || self.prompt.is_some() {
            return None;
        }
        let (shown, full) = self.queued.pop_front()?;
```

with:

```rust

    /// The next queued input, once no turn runs.
    pub fn next_queued(&mut self) -> Option<Action> {
        if self.busy() || self.prompt.is_some() || self.plan_choice.is_some() {
            return None;
        }
        let (shown, full) = self.queued.pop_front()?;
```

Replace (9 of 12):

```rust
        self.context = context;
    }

    /// Asks for a turn.
    fn run(&mut self, input: TurnInput) -> Option<Action> {
        self.running = true;
        Some(Action::Run(input))
    }

    /// Takes in a paste.
```

with:

```rust
        self.context = context;
    }

    /// Asks for a turn. In a session that started in plan mode, the agent is told to plan first.
    fn run(&mut self, input: TurnInput) -> Option<Action> {
        self.running = true;
        if std::mem::take(&mut self.plan_note_pending) && self.mode == Mode::Plan {
            return Some(Action::RunIn(Mode::Plan, input));
        }
        Some(Action::Run(input))
    }

    /// The plan waiting for the user's choice.
    pub fn plan_choice(&self) -> Option<&PlanChoice> {
        self.plan_choice.as_ref()
    }

    /// A plan choice was made.
    fn choose(&mut self, choice: Choice) -> Option<Action> {
        let width = self.width;
        match choice {
            Choice::KeepPlanning => {
                self.plan_choice = None;
                self.transcript
                    .push_note("still planning: say what to change", width);
                None
            }
            Choice::Edit => {
                let plan = self.plan_choice.as_ref()?.plan.clone();
                Some(Action::EditPlan(plan))
            }
            Choice::Build => {
                let choice = self.plan_choice.take()?;
                let mode = self.mode_before_plan.take().unwrap_or(self.default_mode);
                self.mode = mode;
                self.transcript
                    .push_note(&format!("switched to {mode} mode"), width);
                self.transcript.push_user("Build the plan", width);
                self.running = true;
                let input = TurnInput {
                    parts: vec![harness_core::turn::InputPart::Text(choice.build_message())],
                    display: Some("Build the plan".into()),
                    plan: Some(choice.plan),
                    ..TurnInput::default()
                };
                Some(Action::RunIn(mode, input))
            }
        }
    }

    /// The plan came back from the user's editor.
    pub fn plan_edited(&mut self, edited: std::io::Result<String>) {
        let width = self.width;
        match edited {
            Ok(plan) => {
                self.transcript.push_note("the edited plan:", width);
                let theme = self.theme();
                self.transcript
                    .push_lines(crate::markdown::render(&plan, width, &theme), width);
                self.plan_choice = Some(PlanChoice { plan, edited: true });
            }
            Err(e) => self
                .transcript
                .push_error(&format!("could not edit the plan: {e}"), width),
        }
    }

    /// Takes in a paste.
```

Replace (10 of 12):

```rust
            let interrupt = answered == Answered::Interrupt;
            self.answer(answered);
            return interrupt.then_some(Action::Interrupt);
        }
        if ctrl && key.code == KeyCode::Char('d') && self.editor.is_empty() {
            return Some(Action::Quit);
```

with:

```rust
            let interrupt = answered == Answered::Interrupt;
            self.answer(answered);
            return interrupt.then_some(Action::Interrupt);
        }
        if let Some(choice) = &self.plan_choice {
            let choice = choice.key(key)?;
            return self.choose(choice);
        }
        if ctrl && key.code == KeyCode::Char('d') && self.editor.is_empty() {
            return Some(Action::Quit);
```

Replace (11 of 12):

```rust
    }

    fn switch_mode(&mut self, mode: Mode) -> Option<Action> {
        self.mode = mode;
        self.transcript
            .push_note(&format!("switched to {mode} mode"), self.width);
```

with:

```rust
    }

    fn switch_mode(&mut self, mode: Mode) -> Option<Action> {
        if mode == Mode::Plan && self.mode != Mode::Plan {
            self.mode_before_plan = Some(self.mode);
        } else if mode != Mode::Plan {
            self.mode_before_plan = None;
        }
        self.plan_note_pending = false;
        self.mode = mode;
        self.transcript
            .push_note(&format!("switched to {mode} mode"), self.width);
```

Replace (12 of 12):

```rust
            let skip = lines.len().saturating_sub(rows);
            return (lines.split_off(skip), None);
        }
        below.clear();
        let (mut editor, mut cursor) = self.editor.render("› ", width, &theme);
        // What waits to be sent, above the input.
```

with:

```rust
            let skip = lines.len().saturating_sub(rows);
            return (lines.split_off(skip), None);
        }
        if let Some(choice) = &self.plan_choice {
            let mut lines = choice.render(width, &theme);
            lines.extend(below);
            let skip = lines.len().saturating_sub(rows);
            return (lines.split_off(skip), None);
        }
        below.clear();
        let (mut editor, mut cursor) = self.editor.render("› ", width, &theme);
        // What waits to be sent, above the input.
```

In `crates/harness-tui/src/ui.rs`:

Replace (1 of 9):

```rust
    app::{Action, App, Host, Options},
    approval::{Reply, Requests},
    inline::InlineTerminal,
};

/// Whether the session goes on.
```

with:

```rust
    app::{Action, App, Host, Options},
    approval::{Reply, Requests},
    inline::InlineTerminal,
    plan::TextEditor,
};

/// Whether the session goes on.
```

Replace (2 of 9):

```rust
/// Work for the task that owns the agent.
enum Job {
    Turn {
        input: TurnInput,
        cancel: CancellationToken,
    },
    SetMode(harness_core::permission::Mode),
```

with:

```rust
/// Work for the task that owns the agent.
enum Job {
    Turn {
        input: Box<TurnInput>,
        cancel: CancellationToken,
    },
    SetMode(harness_core::permission::Mode),
```

Replace (3 of 9):

```rust
    runner: Option<JoinHandle<()>>,
    /// Cancels the running turn.
    cancel: Option<CancellationToken>,
}

impl<B> Ui<B>
```

with:

```rust
    runner: Option<JoinHandle<()>>,
    /// Cancels the running turn.
    cancel: Option<CancellationToken>,
    text_editor: Option<Box<dyn TextEditor>>,
}

impl<B> Ui<B>
```

Replace (4 of 9):

```rust
        agent: Agent,
        host: Box<dyn Host>,
        term: InlineTerminal<B>,
        options: Options,
        approvals: Requests,
    ) -> Self {
        let (jobs, mut queue) = mpsc::unbounded_channel::<Job>();
        let (events_tx, events) = mpsc::unbounded_channel();
        let (contexts_tx, contexts) = mpsc::unbounded_channel();
```

with:

```rust
        agent: Agent,
        host: Box<dyn Host>,
        term: InlineTerminal<B>,
        mut options: Options,
        approvals: Requests,
    ) -> Self {
        let text_editor = options.text_editor.take();
        let (jobs, mut queue) = mpsc::unbounded_channel::<Job>();
        let (events_tx, events) = mpsc::unbounded_channel();
        let (contexts_tx, contexts) = mpsc::unbounded_channel();
```

Replace (5 of 9):

```rust
            while let Some(job) = queue.recv().await {
                match job {
                    Job::Turn { input, cancel } => {
                        agent.run_turn(input, &events_tx, cancel).await;
                    }
                    Job::SetMode(mode) => agent.set_mode(mode),
                }
```

with:

```rust
            while let Some(job) = queue.recv().await {
                match job {
                    Job::Turn { input, cancel } => {
                        agent.run_turn(*input, &events_tx, cancel).await;
                    }
                    Job::SetMode(mode) => agent.set_mode(mode),
                }
```

Replace (6 of 9):

```rust
            approvals,
            runner: Some(runner),
            cancel: None,
        }
    }

```

with:

```rust
            approvals,
            runner: Some(runner),
            cancel: None,
            text_editor,
        }
    }

```

Replace (7 of 9):

```rust
        })
    }

    fn dispatch(&mut self, action: Action) -> Flow {
        match action {
            Action::Run(input) => {
                let cancel = CancellationToken::new();
                self.cancel = Some(cancel.clone());
                if let Some(jobs) = &self.jobs {
                    let _ = jobs.send(Job::Turn { input, cancel });
                    self.awaiting_context = true;
                }
                Flow::Continue
```

with:

```rust
        })
    }

    fn dispatch(&mut self, action: Action) -> io::Result<Flow> {
        Ok(match action {
            Action::RunIn(mode, input) => {
                self.dispatch(Action::SetMode(mode))?;
                self.dispatch(Action::Run(input))?
            }
            Action::EditPlan(plan) => {
                let edited = match &mut self.text_editor {
                    Some(editor) => {
                        // The editor gets the terminal; the live region is drawn again after.
                        self.term.clear()?;
                        editor.edit(&plan)
                    }
                    None => Err(io::Error::other("no editor is set up")),
                };
                self.app.plan_edited(edited);
                Flow::Continue
            }
            Action::Run(input) => {
                let cancel = CancellationToken::new();
                self.cancel = Some(cancel.clone());
                if let Some(jobs) = &self.jobs {
                    let _ = jobs.send(Job::Turn {
                        input: Box::new(input),
                        cancel,
                    });
                    self.awaiting_context = true;
                }
                Flow::Continue
```

Replace (8 of 9):

```rust
                Flow::Continue
            }
            Action::Quit => Flow::Quit,
        }
    }

    /// Takes in one terminal event: a key, a paste, or a resize.
    pub fn handle(&mut self, event: Event) -> io::Result<Flow> {
        let flow = match event {
            Event::Key(key) => match self.app.on_key(key, Instant::now()) {
                Some(action) => self.dispatch(action),
                None => Flow::Continue,
            },
            Event::Paste(text) => {
```

with:

```rust
                Flow::Continue
            }
            Action::Quit => Flow::Quit,
        })
    }

    /// Takes in one terminal event: a key, a paste, or a resize.
    pub fn handle(&mut self, event: Event) -> io::Result<Flow> {
        let flow = match event {
            Event::Key(key) => match self.app.on_key(key, Instant::now()) {
                Some(action) => self.dispatch(action)?,
                None => Flow::Continue,
            },
            Event::Paste(text) => {
```

Replace (9 of 9):

```rust
            self.app.on_event(&event);
        }
        if let Some(action) = self.app.take_pending_mode() {
            self.dispatch(action);
        }
        if let Some(action) = self.app.next_queued() {
            self.dispatch(action);
        }
        self.draw()?;
        Ok(Flow::Continue)
```

with:

```rust
            self.app.on_event(&event);
        }
        if let Some(action) = self.app.take_pending_mode() {
            self.dispatch(action)?;
        }
        if let Some(action) = self.app.next_queued() {
            self.dispatch(action)?;
        }
        self.draw()?;
        Ok(Flow::Continue)
```

In `crates/harness-tui/src/lib.rs`:

Replace:

```rust
pub mod highlight;
pub mod inline;
pub mod markdown;
pub mod status;
pub mod style;
pub mod terminal;
```

with:

```rust
pub mod highlight;
pub mod inline;
pub mod markdown;
pub mod plan;
pub mod status;
pub mod style;
pub mod terminal;
```

- [ ] **Step 5: The CLI's default mode and editor**

The editor owns the terminal's modes, so they are undone while it runs and when the session ends. In `crates/harness-cli/src/interactive.rs`:

Replace (1 of 3):

```rust
    app::{Host, Options, Prepared},
    approval::{ChannelApprover, Requests},
    inline::InlineTerminal,
    style::Theme,
    terminal::{CrosstermRawMode, Modes},
    ui::Ui,
```

with:

```rust
    app::{Host, Options, Prepared},
    approval::{ChannelApprover, Requests},
    inline::InlineTerminal,
    plan::ExternalEditor,
    style::Theme,
    terminal::{CrosstermRawMode, Modes},
    ui::Ui,
```

Replace (2 of 3):

```rust
        history,
        instruction_files: crate::context::instruction_files(&setup),
        window_note: Some("assumed until model profiles report the model's own".into()),
    };
    let host = CliHost {
        setup: setup.clone(),
```

with:

```rust
        history,
        instruction_files: crate::context::instruction_files(&setup),
        window_note: Some("assumed until model profiles report the model's own".into()),
        // Where Build goes when the session started in plan mode.
        default_mode: setup
            .config
            .mode
            .filter(|m| !matches!(m, Mode::Plan | Mode::ReadOnly))
            .unwrap_or_else(|| config::default_mode(&setup.workspace)),
        text_editor: None,
    };
    let host = CliHost {
        setup: setup.clone(),
```

Replace (3 of 3):

```rust
async fn terminal_session(
    agent: harness_core::agent::Agent,
    host: Box<dyn Host>,
    options: Options,
    approvals: Requests,
) -> std::io::Result<()> {
    // Asked before any events are read: both queries read the terminal's answer from stdin.
    let keyboard = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    let (column, row) = crossterm::cursor::position().unwrap_or((0, 0));
    let top = if column == 0 { row } else { row + 1 };
    let _modes = Modes::enter(std::io::stdout(), CrosstermRawMode, keyboard)?;
    let term = InlineTerminal::new(CrosstermBackend::new(std::io::stdout()), top)?;
    let ui = Ui::start(agent, host, term, options, approvals);
    ui.run(EventStream::new()).await
```

with:

```rust
async fn terminal_session(
    agent: harness_core::agent::Agent,
    host: Box<dyn Host>,
    mut options: Options,
    approvals: Requests,
) -> std::io::Result<()> {
    // Asked before any events are read: both queries read the terminal's answer from stdin.
    let keyboard = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    let (column, row) = crossterm::cursor::position().unwrap_or((0, 0));
    let top = if column == 0 { row } else { row + 1 };
    // The editor for plans owns the terminal's modes, so they are undone while it runs, and
    // when the session ends.
    let modes = Modes::enter(std::io::stdout(), CrosstermRawMode, keyboard)?;
    options.text_editor = Some(Box::new(ExternalEditor::from_env(modes)));
    let term = InlineTerminal::new(CrosstermBackend::new(std::io::stdout()), top)?;
    let ui = Ui::start(agent, host, term, options, approvals);
    ui.run(EventStream::new()).await
```

- [ ] **Step 6: Run the tests, and lint**

Run: `cargo test -p harness-core -p harness-tui -p harness-cli`
Expected: PASS, including `an_edited_plan_is_shown_again_and_built_as_edited` and `the_editor_edits_the_plan_with_the_terminal_given_back_meanwhile`.

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add crates/harness-core crates/harness-tui crates/harness-cli
git commit -F - <<'EOF'
feat(tui): end plan mode with Build, Edit or Keep planning

In plan mode the agent's mode note now asks for a step-by-step
implementation plan, and a completed planning turn shows the plan with
three choices. Build switches back to the mode before plan mode (or the
default mode) and starts a turn implementing it, saving the approved
plan on that turn's message entry (a field older versions ignore);
Edit opens the plan in $EDITOR (vi without one) with the terminal's
modes undone meanwhile and shows the edited plan for approval; Keep
planning stays in plan mode. A session started in plan mode is told
to plan with its first message.

<trailer lines from the controller>
EOF
```

---

### Task 12: Notifications: OSC 9 and the bell

**Files:**
- Modify: `crates/harness-config/src/config.rs`, `crates/harness-tui/src/{app,ui,lib}.rs`, `crates/harness-cli/src/interactive.rs`
- Create: `crates/harness-tui/src/notify.rs`
- Test: `crates/harness-config/tests/notifications.rs`, `crates/harness-tui/tests/notify.rs`, and the other `harness-tui` tests' options

**Interfaces:**
- Consumes: Task 6's `App`, `Ui`; Task 8's approvals.
- Produces:
  - `config::NotificationSettings { desktop, bell }` (`[notifications]`), `config::Notifications { desktop, bell }` (both on by default) and `Config::notifications`;
  - `notify::Notify` (`notify(&str)`), `notify::TerminalNotifier<W>` (`new(out, desktop, bell)`, `out()`), `notify::LONG_TURN` (10 s), `notify::duration(Duration) -> String`;
  - `Options { …, notifier: Option<Box<dyn Notify>> }`, `App::on_event_at(&AgentEvent, Instant)`, `App::take_notifications() -> Vec<String>`.

- [ ] **Step 1: Write the failing tests**

Create `crates/harness-config/tests/notifications.rs`:

```rust
//! `[notifications]`: on by default, each of them can be turned off, and a project may set them
//! without trust.

use harness_config::{
    config::{self, Notifications},
    trust::TrustStore,
};

fn load(global: &str, project: Option<&str>) -> Result<config::Config, config::ConfigError> {
    let dir = tempfile::tempdir().unwrap();
    let global_file = dir.path().join("config.toml");
    std::fs::write(&global_file, global).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(ws.join(".harness")).unwrap();
    if let Some(project) = project {
        std::fs::write(ws.join(".harness/config.toml"), project).unwrap();
    }
    let trust = TrustStore::load(&dir.path().join("data")).unwrap();
    config::load(&global_file, &ws, &trust)
}

#[test]
fn notifications_are_on_by_default() {
    let cfg = load("", None).unwrap();
    assert_eq!(
        cfg.notifications,
        Notifications {
            desktop: true,
            bell: true
        }
    );
}

#[test]
fn each_notification_can_be_turned_off() {
    let cfg = load("[notifications]\ndesktop = false\n", None).unwrap();
    assert_eq!(
        cfg.notifications,
        Notifications {
            desktop: false,
            bell: true
        }
    );
    let cfg = load("[notifications]\nbell = false\n", None).unwrap();
    assert!(cfg.notifications.desktop && !cfg.notifications.bell);
}

#[test]
fn a_project_sets_them_without_trust_or_a_warning() {
    let cfg = load(
        "[notifications]\nbell = false\n",
        Some("[notifications]\nbell = true\ndesktop = false\n"),
    )
    .unwrap();
    assert_eq!(
        cfg.notifications,
        Notifications {
            desktop: false,
            bell: true
        }
    );
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
    // They are not among the settings `harness trust` shows.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".harness")).unwrap();
    std::fs::write(
        dir.path().join(".harness/config.toml"),
        "[notifications]\ndesktop = false\n",
    )
    .unwrap();
    let widening = config::project_widening(&dir.path().join("none.toml"), dir.path()).unwrap();
    assert!(widening.items.is_empty(), "{:?}", widening.items);
}

#[test]
fn an_unknown_notification_setting_is_an_error() {
    let error = load("[notifications]\nsound = true\n", None).unwrap_err();
    assert!(error.to_string().contains("sound"), "{error}");
}
```

Create `crates/harness-tui/tests/notify.rs`:

```rust
//! Notifications: the bytes written to the terminal, and when they are sent.

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use harness_core::{
    agent::{Agent, AgentConfig},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    event::{AgentEvent, TurnEndReason},
    permission::Mode,
    testing::{MockProvider, Script},
    tool::{Tool, ToolContext, ToolRegistry},
};
use harness_tui::{
    app::{App, Host, Options, Prepared},
    approval::ChannelApprover,
    inline::InlineTerminal,
    notify::{self, Notify, TerminalNotifier},
    style::Theme,
    ui::Ui,
};
use ratatui::{
    backend::TestBackend,
    crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers},
};
use serde_json::json;

struct NoCommands;

impl Host for NoCommands {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
}

/// Keeps what it was asked to send.
#[derive(Clone, Default)]
struct Recording(Arc<Mutex<Vec<String>>>);

impl Notify for Recording {
    fn notify(&mut self, text: &str) -> std::io::Result<()> {
        self.0.lock().unwrap().push(text.to_string());
        Ok(())
    }
}

fn options(dir: &Path, notifier: Option<Box<dyn Notify>>) -> Options {
    Options {
        theme: Theme::monochrome(),
        model: "mock/m".into(),
        mode: Mode::Ask,
        commands: Vec::new(),
        workspace: dir.to_path_buf(),
        history: Vec::new(),
        instruction_files: Vec::new(),
        window_note: None,
        default_mode: Mode::Auto,
        text_editor: None,
        notifier,
    }
}

fn sent(notifier: bool, bell: bool, text: &str) -> String {
    let mut out = TerminalNotifier::new(Vec::new(), notifier, bell);
    out.notify(text).unwrap();
    String::from_utf8(out.out().clone()).unwrap()
}

#[test]
fn a_notification_is_an_osc_9_sequence_and_a_bell_each_when_enabled() {
    assert_eq!(
        sent(true, true, "the turn finished after 12s"),
        "\x1b]9;harness: the turn finished after 12s\x07\x07"
    );
    assert_eq!(sent(true, false, "x"), "\x1b]9;harness: x\x07");
    assert_eq!(sent(false, true, "x"), "\x07");
    assert_eq!(sent(false, false, "x"), "");
    // Text from the model cannot end the sequence early or start another.
    assert_eq!(
        sent(true, false, "run `x\x1b]0;evil\x07`\n"),
        "\x1b]9;harness: run `x]0;evil`\x07"
    );
    let long = "y".repeat(500);
    assert_eq!(
        sent(true, false, &long).len(),
        "\x1b]9;harness: \x07".len() + 200
    );
}

#[test]
fn durations_read_as_people_say_them() {
    assert_eq!(notify::duration(Duration::from_secs(12)), "12s");
    assert_eq!(notify::duration(Duration::from_secs(182)), "3m 2s");
    assert_eq!(notify::duration(Duration::from_secs(3_900)), "1h 5m");
}

#[test]
fn a_turn_that_ran_long_notifies_when_it_ends_unless_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = App::new(options(dir.path(), None), Box::new(NoCommands), 80);
    let start = Instant::now();
    let turn = |app: &mut App, took: u64, reason: TurnEndReason| {
        app.on_event_at(&AgentEvent::TurnStarted, start);
        app.on_event_at(
            &AgentEvent::TurnFinished { reason },
            start + Duration::from_secs(took),
        );
        app.take_notifications()
    };
    assert_eq!(
        turn(&mut app, 180, TurnEndReason::Completed),
        ["the turn finished after 3m 0s"]
    );
    assert!(turn(&mut app, 9, TurnEndReason::Completed).is_empty());
    assert!(turn(&mut app, 60, TurnEndReason::Interrupted).is_empty());
    assert_eq!(
        turn(&mut app, 11, TurnEndReason::Error),
        ["the turn stopped with an error after 11s"]
    );
}

#[tokio::test]
async fn an_approval_notifies() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::tool_call("b1", "bash", json!({"command": "echo hi"})),
        Script::text("done"),
    ]);
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode: Mode::Ask,
        workspace: dir.path().to_path_buf(),
        read_dirs: Vec::new(),
        rules: RuleSet::default(),
        sandbox_available: true,
        writes_need_approval: false,
    }));
    let bash: Arc<dyn Tool> = harness_tools::builtin().get("bash").unwrap();
    let (approver, approvals) = ChannelApprover::new();
    let agent = Agent::new(
        provider,
        ToolRegistry::new(vec![bash]),
        policy,
        approver,
        AgentConfig::new("mock/m", "m", "system", dir.path().join("out")),
        ToolContext::new(dir.path()),
    );
    let recording = Recording::default();
    let term = InlineTerminal::new(TestBackend::new(80, 20), 0).unwrap();
    let mut ui = Ui::start(
        agent,
        Box::new(NoCommands),
        term,
        options(dir.path(), Some(Box::new(recording.clone()))),
        approvals,
    );
    for c in "go".chars() {
        ui.handle(Event::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )))
        .unwrap();
    }
    ui.handle(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
    .unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        ui.until(|app| app.prompt().is_some()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        *recording.0.lock().unwrap(),
        ["approval needed: run `echo hi`"]
    );
    ui.handle(Event::Key(KeyEvent::new(
        KeyCode::Char('y'),
        KeyModifiers::NONE,
    )))
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), ui.settle())
        .await
        .unwrap()
        .unwrap();
    // A short turn does not notify when it ends.
    assert_eq!(recording.0.lock().unwrap().len(), 1);
    ui.finish().await.unwrap();
}
```

The other sessions' options gain the notifier. In `crates/harness-tui/tests/session.rs`:

Replace:

```rust
        window_note: None,
        default_mode: Mode::Auto,
        text_editor: None,
    }
}

```

with:

```rust
        window_note: None,
        default_mode: Mode::Auto,
        text_editor: None,
        notifier: None,
    }
}

```

In `crates/harness-tui/tests/status.rs`:

Replace:

```rust
        window_note: Some("assumed".into()),
        default_mode: Mode::Auto,
        text_editor: None,
    };
    let term = InlineTerminal::new(TestBackend::new(70, 20), 0).unwrap();
    let mut ui = Ui::start(
```

with:

```rust
        window_note: Some("assumed".into()),
        default_mode: Mode::Auto,
        text_editor: None,
        notifier: None,
    };
    let term = InlineTerminal::new(TestBackend::new(70, 20), 0).unwrap();
    let mut ui = Ui::start(
```

In `crates/harness-tui/tests/approvals.rs`:

Replace:

```rust
        window_note: None,
        default_mode: Mode::Auto,
        text_editor: None,
    };
    let term = InlineTerminal::new(TestBackend::new(80, 24), 0).unwrap();
    let mut ui = Ui::start(agent, Box::new(NoCommands), term, options, approvals);
```

with:

```rust
        window_note: None,
        default_mode: Mode::Auto,
        text_editor: None,
        notifier: None,
    };
    let term = InlineTerminal::new(TestBackend::new(80, 24), 0).unwrap();
    let mut ui = Ui::start(agent, Box::new(NoCommands), term, options, approvals);
```

In `crates/harness-tui/tests/modes.rs`:

Replace:

```rust
        window_note: None,
        default_mode: Mode::Auto,
        text_editor: None,
    };
    let term = InlineTerminal::new(TestBackend::new(100, 20), 0).unwrap();
    let mut ui = Ui::start(
```

with:

```rust
        window_note: None,
        default_mode: Mode::Auto,
        text_editor: None,
        notifier: None,
    };
    let term = InlineTerminal::new(TestBackend::new(100, 20), 0).unwrap();
    let mut ui = Ui::start(
```

In `crates/harness-tui/tests/steering.rs`:

Replace:

```rust
        window_note: None,
        default_mode: Mode::Auto,
        text_editor: None,
    };
    let term = InlineTerminal::new(TestBackend::new(80, 24), 0).unwrap();
    let mut ui = Ui::start(
```

with:

```rust
        window_note: None,
        default_mode: Mode::Auto,
        text_editor: None,
        notifier: None,
    };
    let term = InlineTerminal::new(TestBackend::new(80, 24), 0).unwrap();
    let mut ui = Ui::start(
```

In `crates/harness-tui/tests/plan.rs`:

Replace:

```rust
        window_note: None,
        default_mode,
        text_editor: Some(Box::new(DeleteStepThree)),
    };
    let term = InlineTerminal::new(TestBackend::new(100, 30), 0).unwrap();
    let mut ui = Ui::start(agent, Box::new(NoCommands), term, options, approvals);
```

with:

```rust
        window_note: None,
        default_mode,
        text_editor: Some(Box::new(DeleteStepThree)),
        notifier: None,
    };
    let term = InlineTerminal::new(TestBackend::new(100, 30), 0).unwrap();
    let mut ui = Ui::start(agent, Box::new(NoCommands), term, options, approvals);
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p harness-config --test notifications`
Expected: FAIL to compile: ``unresolved import `harness_config::config::Notifications` `` and ``no field `notifications` on type `Config` ``.

Run: `cargo test -p harness-tui --test notify`
Expected: FAIL to compile: ``unresolved import `harness_tui::notify` ``.

- [ ] **Step 3: `[notifications]`**

In `crates/harness-config/src/config.rs`:

Replace (1 of 5):

```rust
    }
}

/// One `config.toml` file as written by the user.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
```

with:

```rust
    }
}

/// `[notifications]`: what the interactive session does when a long turn ends or an approval
/// waits. A project may set it without trust: it changes nothing the agent may do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationSettings {
    /// A desktop notification through the terminal (OSC 9); on unless set to `false`.
    pub desktop: Option<bool>,
    /// The terminal bell; on unless set to `false`.
    pub bell: Option<bool>,
}

/// The notifications in effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Notifications {
    pub desktop: bool,
    pub bell: bool,
}

impl Default for Notifications {
    fn default() -> Self {
        Notifications {
            desktop: true,
            bell: true,
        }
    }
}

impl Notifications {
    /// These, with what `settings` sets.
    fn overlaid(self, settings: &NotificationSettings) -> Notifications {
        Notifications {
            desktop: settings.desktop.unwrap_or(self.desktop),
            bell: settings.bell.unwrap_or(self.bell),
        }
    }
}

/// One `config.toml` file as written by the user.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
```

Replace (2 of 5):

```rust
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub compaction: CompactionSettings,
}

#[derive(Debug, thiserror::Error)]
```

with:

```rust
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub compaction: CompactionSettings,
    #[serde(default)]
    pub notifications: NotificationSettings,
}

#[derive(Debug, thiserror::Error)]
```

Replace (3 of 5):

```rust
    pub writable_roots: Vec<PathBuf>,
    pub allow_localhost: bool,
    pub linux_git_protection: LinuxGitProtection,
    /// Whether the user trusted this workspace with its project settings as they are now
    /// (`harness trust`), so that their widening settings apply. A workspace with no such
    /// settings can be trusted too. A project command file's `model` applies only then.
```

with:

```rust
    pub writable_roots: Vec<PathBuf>,
    pub allow_localhost: bool,
    pub linux_git_protection: LinuxGitProtection,
    pub notifications: Notifications,
    /// Whether the user trusted this workspace with its project settings as they are now
    /// (`harness trust`), so that their widening settings apply. A workspace with no such
    /// settings can be trusted too. A project command file's `model` applies only then.
```

Replace (4 of 5):

```rust
        cfg.writable_roots = expand_all(&global.sandbox.writable_roots, base, home);
        cfg.allow_localhost = global.sandbox.allow_localhost.unwrap_or(false);
        cfg.linux_git_protection = global.sandbox.linux_git_protection.unwrap_or_default();
    }
    let path = project_file(workspace);
    let project = parse_file(&path)?;
```

with:

```rust
        cfg.writable_roots = expand_all(&global.sandbox.writable_roots, base, home);
        cfg.allow_localhost = global.sandbox.allow_localhost.unwrap_or(false);
        cfg.linux_git_protection = global.sandbox.linux_git_protection.unwrap_or_default();
        cfg.notifications = cfg.notifications.overlaid(&global.notifications);
    }
    let path = project_file(workspace);
    let project = parse_file(&path)?;
```

Replace (5 of 5):

```rust
        if let Some(message) = project.compaction.out_of_range() {
            return Err(ConfigError::Parse { path, message });
        }
        cfg.deny.extend(project.permissions.deny.iter().cloned());
        cfg.confirm
            .extend(project.permissions.confirm.iter().cloned());
```

with:

```rust
        if let Some(message) = project.compaction.out_of_range() {
            return Err(ConfigError::Parse { path, message });
        }
        cfg.notifications = cfg.notifications.overlaid(&project.notifications);
        cfg.deny.extend(project.permissions.deny.iter().cloned());
        cfg.confirm
            .extend(project.permissions.confirm.iter().cloned());
```

Run: `cargo test -p harness-config`
Expected: PASS.

- [ ] **Step 4: The notifier, and when it is used**

Create `crates/harness-tui/src/notify.rs`:

```rust
//! Telling the user something needs them while they look elsewhere: a desktop notification
//! through the terminal (OSC 9) and the terminal bell, when a turn that ran long finishes, or
//! an approval waits.

use std::{
    io::{self, Write},
    time::Duration,
};

/// Turns that run at least this long notify when they finish.
pub const LONG_TURN: Duration = Duration::from_secs(10);

/// Characters of a notification's text at most.
const MAX_TEXT: usize = 200;

/// Sends notifications.
pub trait Notify: Send {
    fn notify(&mut self, text: &str) -> io::Result<()>;
}

/// Notifications written to the terminal: OSC 9 (`ESC ] 9 ; text BEL`) and a bell, each when
/// enabled. The session enables them only when stdout is a terminal.
pub struct TerminalNotifier<W: Write + Send> {
    out: W,
    desktop: bool,
    bell: bool,
}

impl<W: Write + Send> TerminalNotifier<W> {
    pub fn new(out: W, desktop: bool, bell: bool) -> Self {
        TerminalNotifier { out, desktop, bell }
    }

    pub fn out(&self) -> &W {
        &self.out
    }
}

/// `text` as an OSC 9 payload: control characters (which would end the sequence early, or start
/// another) are dropped, and it is cut to [`MAX_TEXT`] characters.
fn payload(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(MAX_TEXT)
        .collect()
}

impl<W: Write + Send> Notify for TerminalNotifier<W> {
    fn notify(&mut self, text: &str) -> io::Result<()> {
        if self.desktop {
            // Starting with "harness:" keeps a terminal from reading the text as one of the
            // numbered OSC 9 commands (ConEmu's `9;4;…` progress, for one).
            write!(self.out, "\x1b]9;harness: {}\x07", payload(text))?;
        }
        if self.bell {
            self.out.write_all(b"\x07")?;
        }
        self.out.flush()
    }
}

/// How long `elapsed` is, for people: `12s`, `3m 2s`, `1h 5m`.
pub fn duration(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, (secs % 3600) / 60),
    }
}
```

In `crates/harness-tui/src/app.rs`:

Replace (1 of 7):

```rust
    approval::{Answered, Prompt, Reply},
    complete::{self, Completer, Offer},
    editor::{Edit, Editor},
    plan::{Choice, PlanChoice, TextEditor},
    status::{self, Totals},
    style::Theme,
```

with:

```rust
    approval::{Answered, Prompt, Reply},
    complete::{self, Completer, Offer},
    editor::{Edit, Editor},
    notify::{self, Notify},
    plan::{Choice, PlanChoice, TextEditor},
    status::{self, Totals},
    style::Theme,
```

Replace (2 of 7):

```rust
    pub default_mode: Mode,
    /// Opens a plan in the user's editor.
    pub text_editor: Option<Box<dyn TextEditor>>,
}

/// What the session should do after a key.
```

with:

```rust
    pub default_mode: Mode,
    /// Opens a plan in the user's editor.
    pub text_editor: Option<Box<dyn TextEditor>>,
    /// Tells the user when a long turn ends or an approval waits.
    pub notifier: Option<Box<dyn Notify>>,
}

/// What the session should do after a key.
```

Replace (3 of 7):

```rust
    plan_note_pending: bool,
    /// A plan waiting for the user's choice.
    plan_choice: Option<PlanChoice>,
    workspace: std::path::PathBuf,
    width: usize,
}
```

with:

```rust
    plan_note_pending: bool,
    /// A plan waiting for the user's choice.
    plan_choice: Option<PlanChoice>,
    /// When the running turn started.
    turn_started: Option<Instant>,
    /// Notifications to send.
    notifications: Vec<String>,
    workspace: std::path::PathBuf,
    width: usize,
}
```

Replace (4 of 7):

```rust
            default_mode: options.default_mode,
            plan_note_pending: options.mode == Mode::Plan,
            plan_choice: None,
            workspace: options.workspace,
            width,
        }
```

with:

```rust
            default_mode: options.default_mode,
            plan_note_pending: options.mode == Mode::Plan,
            plan_choice: None,
            turn_started: None,
            notifications: Vec::new(),
            workspace: options.workspace,
            width,
        }
```

Replace (5 of 7):

```rust
        self.width = width;
    }

    /// The agent asks the user to approve `request`.
    pub fn on_approval(&mut self, request: ApprovalRequest, reply: Reply) {
        let arguments = self.transcript.arguments(&request.call_id).cloned();
        let theme = self.theme();
        self.prompt = Some(Prompt::new(
```

with:

```rust
        self.width = width;
    }

    /// Notifications to send now, which are then forgotten.
    pub fn take_notifications(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notifications)
    }

    /// The agent asks the user to approve `request`.
    pub fn on_approval(&mut self, request: ApprovalRequest, reply: Reply) {
        self.notifications
            .push(format!("approval needed: {}", request.reason));
        let arguments = self.transcript.arguments(&request.call_id).cloned();
        let theme = self.theme();
        self.prompt = Some(Prompt::new(
```

Replace (6 of 7):

```rust

    /// Takes in an event from the agent.
    pub fn on_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::TurnStarted => self.last_reply.clear(),
            AgentEvent::AssistantMessage { content, .. } if !content.trim().is_empty() => {
                self.last_reply = content.clone();
            }
            AgentEvent::TurnFinished { reason } => self.turn_ended(*reason),
            AgentEvent::Usage { model, usage } => self.totals.add(model, usage),
            AgentEvent::Steered { text } => {
                if let Some(i) = self.sent_now.iter().position(|t| t == text) {
```

with:

```rust

    /// Takes in an event from the agent.
    pub fn on_event(&mut self, event: &AgentEvent) {
        self.on_event_at(event, Instant::now());
    }

    /// Takes in an event from the agent that came at `now`.
    pub fn on_event_at(&mut self, event: &AgentEvent, now: Instant) {
        match event {
            AgentEvent::TurnStarted => {
                self.last_reply.clear();
                self.turn_started = Some(now);
            }
            AgentEvent::AssistantMessage { content, .. } if !content.trim().is_empty() => {
                self.last_reply = content.clone();
            }
            AgentEvent::TurnFinished { reason } => self.turn_ended(*reason, now),
            AgentEvent::Usage { model, usage } => self.totals.add(model, usage),
            AgentEvent::Steered { text } => {
                if let Some(i) = self.sent_now.iter().position(|t| t == text) {
```

Replace (7 of 7):

```rust

    /// A turn ended. Send-now input it did not take is sent next, before queued input. After an
    /// interruption, both go back into the editor instead, for the user to look at again.
    fn turn_ended(&mut self, reason: TurnEndReason) {
        self.running = false;
        if reason == TurnEndReason::Completed
            && self.mode == Mode::Plan
            && !self.last_reply.trim().is_empty()
```

with:

```rust

    /// A turn ended. Send-now input it did not take is sent next, before queued input. After an
    /// interruption, both go back into the editor instead, for the user to look at again.
    fn turn_ended(&mut self, reason: TurnEndReason, now: Instant) {
        self.running = false;
        if let Some(started) = self.turn_started.take() {
            let took = now.saturating_duration_since(started);
            let how = match reason {
                TurnEndReason::Completed => Some("the turn finished"),
                TurnEndReason::Error => Some("the turn stopped with an error"),
                TurnEndReason::StepLimit => Some("the turn stopped at the step limit"),
                TurnEndReason::Interrupted => None,
            };
            if let Some(how) = how.filter(|_| took >= notify::LONG_TURN) {
                self.notifications
                    .push(format!("{how} after {}", notify::duration(took)));
            }
        }
        if reason == TurnEndReason::Completed
            && self.mode == Mode::Plan
            && !self.last_reply.trim().is_empty()
```

In `crates/harness-tui/src/ui.rs`:

Replace (1 of 5):

```rust
    app::{Action, App, Host, Options},
    approval::{Reply, Requests},
    inline::InlineTerminal,
    plan::TextEditor,
};

```

with:

```rust
    app::{Action, App, Host, Options},
    approval::{Reply, Requests},
    inline::InlineTerminal,
    notify::Notify,
    plan::TextEditor,
};

```

Replace (2 of 5):

```rust
    /// Cancels the running turn.
    cancel: Option<CancellationToken>,
    text_editor: Option<Box<dyn TextEditor>>,
}

impl<B> Ui<B>
```

with:

```rust
    /// Cancels the running turn.
    cancel: Option<CancellationToken>,
    text_editor: Option<Box<dyn TextEditor>>,
    notifier: Option<Box<dyn Notify>>,
}

impl<B> Ui<B>
```

Replace (3 of 5):

```rust
        approvals: Requests,
    ) -> Self {
        let text_editor = options.text_editor.take();
        let (jobs, mut queue) = mpsc::unbounded_channel::<Job>();
        let (events_tx, events) = mpsc::unbounded_channel();
        let (contexts_tx, contexts) = mpsc::unbounded_channel();
```

with:

```rust
        approvals: Requests,
    ) -> Self {
        let text_editor = options.text_editor.take();
        let notifier = options.notifier.take();
        let (jobs, mut queue) = mpsc::unbounded_channel::<Job>();
        let (events_tx, events) = mpsc::unbounded_channel();
        let (contexts_tx, contexts) = mpsc::unbounded_channel();
```

Replace (4 of 5):

```rust
            runner: Some(runner),
            cancel: None,
            text_editor,
        }
    }

```

with:

```rust
            runner: Some(runner),
            cancel: None,
            text_editor,
            notifier,
        }
    }

```

Replace (5 of 5):

```rust
        &mut self.term
    }

    /// Writes the finished lines into the scrollback and redraws the live region.
    pub fn draw(&mut self) -> io::Result<()> {
        let finished = self.app.transcript.take_finished();
        self.term.insert(&finished)?;
        let rows = self.term.height() as usize;
```

with:

```rust
        &mut self.term
    }

    /// Sends the notifications waiting. One that fails is dropped: it is no reason to stop.
    fn notify(&mut self) {
        for text in self.app.take_notifications() {
            if let Some(notifier) = &mut self.notifier {
                let _ = notifier.notify(&text);
            }
        }
    }

    /// Writes the finished lines into the scrollback and redraws the live region.
    pub fn draw(&mut self) -> io::Result<()> {
        self.notify();
        let finished = self.app.transcript.take_finished();
        self.term.insert(&finished)?;
        let rows = self.term.height() as usize;
```

In `crates/harness-tui/src/lib.rs`:

Replace:

```rust
pub mod highlight;
pub mod inline;
pub mod markdown;
pub mod plan;
pub mod status;
pub mod style;
```

with:

```rust
pub mod highlight;
pub mod inline;
pub mod markdown;
pub mod notify;
pub mod plan;
pub mod status;
pub mod style;
```

- [ ] **Step 5: The CLI writes them to the terminal**

The interactive session starts only with stdout a terminal, so the notifier is enabled only then. In `crates/harness-cli/src/interactive.rs`:

Replace (1 of 4):

```rust
    app::{Host, Options, Prepared},
    approval::{ChannelApprover, Requests},
    inline::InlineTerminal,
    plan::ExternalEditor,
    style::Theme,
    terminal::{CrosstermRawMode, Modes},
```

with:

```rust
    app::{Host, Options, Prepared},
    approval::{ChannelApprover, Requests},
    inline::InlineTerminal,
    notify::TerminalNotifier,
    plan::ExternalEditor,
    style::Theme,
    terminal::{CrosstermRawMode, Modes},
```

Replace (2 of 4):

```rust
            .filter(|m| !matches!(m, Mode::Plan | Mode::ReadOnly))
            .unwrap_or_else(|| config::default_mode(&setup.workspace)),
        text_editor: None,
    };
    let host = CliHost {
        setup: setup.clone(),
        commands,
        policy,
    };
    let result = terminal_session(agent, Box::new(host), options, approvals).await;
    sandbox_session.end();
    match result {
        Ok(()) => 0,
```

with:

```rust
            .filter(|m| !matches!(m, Mode::Plan | Mode::ReadOnly))
            .unwrap_or_else(|| config::default_mode(&setup.workspace)),
        text_editor: None,
        notifier: None,
    };
    let host = CliHost {
        setup: setup.clone(),
        commands,
        policy,
    };
    let notifications = setup.config.notifications;
    let result = terminal_session(agent, Box::new(host), options, approvals, notifications).await;
    sandbox_session.end();
    match result {
        Ok(()) => 0,
```

Replace (3 of 4):

```rust
    host: Box<dyn Host>,
    mut options: Options,
    approvals: Requests,
) -> std::io::Result<()> {
    // Asked before any events are read: both queries read the terminal's answer from stdin.
    let keyboard = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
```

with:

```rust
    host: Box<dyn Host>,
    mut options: Options,
    approvals: Requests,
    notifications: harness_config::config::Notifications,
) -> std::io::Result<()> {
    // Asked before any events are read: both queries read the terminal's answer from stdin.
    let keyboard = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
```

Replace (4 of 4):

```rust
    // when the session ends.
    let modes = Modes::enter(std::io::stdout(), CrosstermRawMode, keyboard)?;
    options.text_editor = Some(Box::new(ExternalEditor::from_env(modes)));
    let term = InlineTerminal::new(CrosstermBackend::new(std::io::stdout()), top)?;
    let ui = Ui::start(agent, host, term, options, approvals);
    ui.run(EventStream::new()).await
```

with:

```rust
    // when the session ends.
    let modes = Modes::enter(std::io::stdout(), CrosstermRawMode, keyboard)?;
    options.text_editor = Some(Box::new(ExternalEditor::from_env(modes)));
    options.notifier = Some(Box::new(TerminalNotifier::new(
        std::io::stdout(),
        notifications.desktop,
        notifications.bell,
    )));
    let term = InlineTerminal::new(CrosstermBackend::new(std::io::stdout()), top)?;
    let ui = Ui::start(agent, host, term, options, approvals);
    ui.run(EventStream::new()).await
```

- [ ] **Step 6: Run the tests, and lint**

Run: `cargo test -p harness-config -p harness-tui -p harness-cli`
Expected: PASS, including `a_notification_is_an_osc_9_sequence_and_a_bell_each_when_enabled` and `an_approval_notifies`.

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add crates/harness-config crates/harness-tui crates/harness-cli
git commit -F - <<'EOF'
feat(tui): notify with OSC 9 and the bell

When a turn that ran 10 seconds or longer ends (other than by the
user's interruption), or an approval waits, the interactive session
writes an OSC 9 desktop notification and a bell to the terminal. The
new [notifications] settings, desktop and bell, turn each off; a
project may set them without trust. The text is stripped of control
characters so it cannot end the sequence early.

<trailer lines from the controller>
EOF
```

---

### Task 13: The first-use trust question

**Files:**
- Modify: `crates/harness-cli/src/trust.rs`, `crates/harness-cli/src/interactive.rs`
- Test: `trust.rs`'s test module

**Interfaces:**
- Consumes: P2's `config::project_widening`, `TrustStore`.
- Produces: `trust::first_use(workspace, paths, answer: &mut dyn BufRead, out: &mut dyn Write) -> io::Result<FirstUse>`, `trust::FirstUse { NothingToAsk, Trusted, Declined }`, and `trust::describe_settings(workspace, items) -> Vec<String>`, which `harness trust` now uses too.

- [ ] **Step 1: Write the failing tests**

Append to `crates/harness-cli/src/trust.rs`, after a blank line:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    struct Workspace {
        _dir: tempfile::TempDir,
        ws: std::path::PathBuf,
        paths: Paths,
    }

    fn workspace(project: &str) -> Workspace {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(ws.join(".harness")).unwrap();
        if !project.is_empty() {
            std::fs::write(ws.join(".harness/config.toml"), project).unwrap();
        }
        let paths = Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
            state_dir: dir.path().join("state"),
        };
        Workspace {
            _dir: dir,
            ws,
            paths,
        }
    }

    fn ask(w: &Workspace, reply: &str) -> (FirstUse, String) {
        let mut out = Vec::new();
        let result = first_use(&w.ws, &w.paths, &mut reply.as_bytes(), &mut out).unwrap();
        (result, String::from_utf8(out).unwrap())
    }

    fn allowed(w: &Workspace) -> Vec<String> {
        let trust = TrustStore::load(&w.paths.data_dir).unwrap();
        config::load(&w.paths.global_config_file(), &w.ws, &trust)
            .unwrap()
            .allow
    }

    const ALLOW: &str = "[permissions]\nallow = [\"bash:make *\"]\n";

    #[test]
    fn trusting_on_first_use_applies_the_settings() {
        let w = workspace(ALLOW);
        assert!(allowed(&w).is_empty());
        let (result, shown) = ask(&w, "y\n");
        assert_eq!(result, FirstUse::Trusted);
        assert!(
            shown.contains("contains settings that need trust:"),
            "{shown}"
        );
        assert!(
            shown.contains("  - permissions.allow: \"bash:make *\""),
            "{shown}"
        );
        assert!(shown.contains("Trust this workspace, so these settings apply? [y/N]"));
        assert_eq!(allowed(&w), ["bash:make *"]);
        // Asked once: trusted now.
        assert_eq!(ask(&w, "").0, FirstUse::NothingToAsk);
    }

    #[test]
    fn declining_leaves_them_off_and_asks_again_next_time() {
        let w = workspace(ALLOW);
        let (result, shown) = ask(&w, "\n");
        assert_eq!(result, FirstUse::Declined);
        assert!(shown.contains("Run `harness trust`"), "{shown}");
        assert!(allowed(&w).is_empty());
        assert_eq!(ask(&w, "no\n").0, FirstUse::Declined);
    }

    #[test]
    fn nothing_is_asked_without_widening_settings() {
        let w = workspace("[permissions]\ndeny = [\"bash:curl *\"]\n");
        assert_eq!(ask(&w, "y\n"), (FirstUse::NothingToAsk, String::new()));
        let w = workspace("");
        assert_eq!(ask(&w, "y\n"), (FirstUse::NothingToAsk, String::new()));
    }

    #[test]
    fn settings_from_the_repository_are_shown_escaped() {
        let w = workspace("[permissions]\nallow = [\"bash:\\u001b[2Jx\"]\n");
        let (_, shown) = ask(&w, "n\n");
        assert!(!shown.contains('\u{1b}'), "{shown:?}");
    }
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p harness-cli --bin harness trust`
Expected: FAIL to compile: ``cannot find function `first_use` in this scope`` and ``cannot find type `FirstUse` in this scope``.

- [ ] **Step 3: The question**

In `crates/harness-cli/src/trust.rs`:

Replace (1 of 2):

```rust
use std::io::{BufRead, IsTerminal, Write};

use harness_config::{config, paths::Paths, trust::TrustStore};
use harness_context::project::project_root;

use crate::term::terminal_safe;

pub fn run(yes: bool, revoke: bool) -> u8 {
    let workspace = match std::env::current_dir().and_then(|d| d.canonicalize()) {
```

with:

```rust
use std::{
    io::{BufRead, IsTerminal, Write},
    path::Path,
};

use harness_config::{config, paths::Paths, trust::TrustStore};
use harness_context::project::project_root;

use crate::term::terminal_safe;

/// The lines that list a workspace's widening settings, as `harness trust` and the interactive
/// session's first-use prompt show them.
pub fn describe_settings(workspace: &Path, items: &[String]) -> Vec<String> {
    let mut lines = vec![format!(
        "{} contains settings that need trust:",
        terminal_safe(&config::project_file(workspace).display().to_string())
    )];
    lines.extend(
        items
            .iter()
            .map(|item| format!("  - {}", terminal_safe(item))),
    );
    lines
}

/// How the first-use prompt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirstUse {
    /// The workspace has no widening settings, or they are trusted already: nothing was asked.
    NothingToAsk,
    Trusted,
    Declined,
}

/// On interactive use of `workspace` while its project settings that widen what the agent may do
/// are not trusted, shows them on `out` and asks whether to trust the workspace, reading the
/// answer from `answer`. Trusting records it as `harness trust` does; declining is asked again
/// next time. Settings that cannot be read are left for loading the configuration to report.
pub fn first_use(
    workspace: &Path,
    paths: &Paths,
    answer: &mut dyn BufRead,
    out: &mut dyn Write,
) -> std::io::Result<FirstUse> {
    let Ok(widening) = config::project_widening(&paths.global_config_file(), workspace) else {
        return Ok(FirstUse::NothingToAsk);
    };
    let Ok(mut store) = TrustStore::load(&paths.data_dir) else {
        return Ok(FirstUse::NothingToAsk);
    };
    if widening.items.is_empty() || store.is_trusted(workspace, &widening.fingerprint) {
        return Ok(FirstUse::NothingToAsk);
    }
    for line in describe_settings(workspace, &widening.items) {
        writeln!(out, "{line}")?;
    }
    write!(out, "Trust this workspace, so these settings apply? [y/N] ")?;
    out.flush()?;
    let mut reply = String::new();
    answer.read_line(&mut reply)?;
    if !matches!(reply.trim(), "y" | "Y" | "yes") {
        writeln!(
            out,
            "Not trusted: harness runs without these settings. Run `harness trust` to review them again."
        )?;
        return Ok(FirstUse::Declined);
    }
    store
        .trust(workspace, &widening.fingerprint)
        .map_err(std::io::Error::other)?;
    writeln!(
        out,
        "Trusted {}.",
        terminal_safe(&workspace.display().to_string())
    )?;
    Ok(FirstUse::Trusted)
}

pub fn run(yes: bool, revoke: bool) -> u8 {
    let workspace = match std::env::current_dir().and_then(|d| d.canonicalize()) {
```

Replace (2 of 2):

```rust
            terminal_safe(&workspace.display().to_string())
        );
    } else {
        println!(
            "{} contains settings that need trust:",
            terminal_safe(&config::project_file(&workspace).display().to_string())
        );
        for item in &widening.items {
            println!("  - {}", terminal_safe(item));
        }
    }
    if root != workspace {
```

with:

```rust
            terminal_safe(&workspace.display().to_string())
        );
    } else {
        for line in describe_settings(&workspace, &widening.items) {
            println!("{line}");
        }
    }
    if root != workspace {
```

It comes before the configuration loads, so settings trusted now apply at once. In `crates/harness-cli/src/interactive.rs`:

Replace:

```rust
    ) {
        eprintln!("error: {message}");
        return 2;
    }
    let setup = match setup::load() {
        Ok(setup) => Arc::new(setup),
```

with:

```rust
    ) {
        eprintln!("error: {message}");
        return 2;
    }
    // Before the configuration loads, so that settings trusted now apply at once.
    if let (Ok(workspace), Ok(paths)) = (
        std::env::current_dir().and_then(|d| d.canonicalize()),
        harness_config::paths::Paths::from_process_env(),
    ) {
        let asked = crate::trust::first_use(
            &workspace,
            &paths,
            &mut std::io::stdin().lock(),
            &mut std::io::stdout(),
        );
        if let Err(e) = asked {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
            return 1;
        }
    }
    let setup = match setup::load() {
        Ok(setup) => Arc::new(setup),
```

- [ ] **Step 4: Run the tests, and lint**

Run: `cargo test -p harness-cli`
Expected: PASS, including `trusting_on_first_use_applies_the_settings`; `trust_e2e` unchanged.

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/harness-cli
git commit -F - <<'EOF'
feat(cli): ask to trust a workspace on first interactive use

Before an interactive session loads its configuration, a workspace
whose project settings would widen what the agent may do, and are not
trusted, gets them listed and one question. Yes records trust as
`harness trust` does, so the settings apply at once; no keeps them off
and asks again next time. Headless runs still ignore them with a
warning.

<trailer lines from the controller>
EOF
```

---

### Task 14: 2.13's M1 and M4, and the README

**Files:**
- Modify: `crates/harness-core/src/{tool,agent}.rs`, `crates/harness-sandbox/src/guard/{mod,snapshot,tests}.rs`, `crates/harness-sandbox/src/linux/{mod,inotify}.rs`, `crates/harness-sandbox/src/watch.rs`, `README.md`
- Test: `crates/harness-core/tests/unsandboxed.rs`, `crates/harness-sandbox/tests/git_guard.rs`, the guard's and the Linux sandbox's test modules

**Interfaces:**
- Consumes: P2.13's `GuardSession::begin`, `Snapshot::take`, `LinuxSandbox`; Task 8's `ApprovalKind::RunUnsandboxed`.
- Produces:
  - `CommandSandbox::cannot_run(&self, access: FsAccess) -> Option<String>` (default `None`), implemented by `LinuxSandbox` for a writing command once the session dropped to the basic tier with `require_full_git_protection`; the agent then asks whether to run the command outside the sandbox;
  - `GuardSession::begin(workspace, placeholders)` and `Snapshot::take(tree, roots, leave_out)`, without `save_all` and `everything`.

- [ ] **Step 1: Write the failing tests**

Create `crates/harness-core/tests/unsandboxed.rs`:

```rust
//! A sandbox that can no longer run a command (Linux, with git protection required, after the
//! session dropped to the basic tier): the command runs outside it only once approved, and a
//! headless run counts it as blocked.

mod common;

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use common::run;
use harness_core::{
    agent::{
        Agent, AgentConfig, ApprovalDecision, ApprovalKind, ApprovalRequest, Approver,
        NonInteractive,
    },
    engine::{EngineConfig, PermissionEngine, RuleSet},
    event::AgentEvent,
    message::ToolSpec,
    permission::{Action, FsAccess, Mode},
    testing::{MockProvider, Script},
    tool::{CommandSandbox, Tool, ToolContext, ToolOutput, ToolRegistry},
};
use serde_json::{Value, json};

/// Refuses commands that may write, as the Linux sandbox does after such a drop.
#[derive(Debug)]
struct Dropped;

impl CommandSandbox for Dropped {
    fn name(&self) -> &'static str {
        "dropped"
    }
    fn command(
        &self,
        _access: FsAccess,
        _workspace: &Path,
        program: &str,
        _args: &[&str],
    ) -> std::io::Result<tokio::process::Command> {
        Ok(tokio::process::Command::new(program))
    }
    fn is_denial(&self, _exit_code: Option<i32>, _output: &str) -> bool {
        false
    }
    fn cannot_run(&self, access: FsAccess) -> Option<String> {
        (access == FsAccess::WorkspaceWrite).then(|| "the sandbox dropped to the basic tier".into())
    }
}

/// Reports whether it ran outside the sandbox.
struct Shell;

#[async_trait]
impl Tool for Shell {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "shell".into(),
            parameters: json!({"type": "object", "properties": {"command": {"type": "string"}}}),
        }
    }
    fn action(&self, args: &Value, _ctx: &ToolContext) -> Action {
        Action::Bash(args["command"].as_str().unwrap_or_default().to_string())
    }
    async fn run(&self, _args: Value, ctx: &ToolContext) -> ToolOutput {
        ToolOutput::ok(format!("ran, unsandboxed={}", ctx.unsandboxed))
    }
}

/// Answers every request with `decision`, and records what it was asked.
struct Answer(ApprovalDecision, Mutex<Vec<ApprovalRequest>>);

#[async_trait]
impl Approver for Answer {
    async fn decide(&self, request: &ApprovalRequest) -> ApprovalDecision {
        self.1.lock().unwrap().push(request.clone());
        self.0.clone()
    }
}

fn agent(mode: Mode, approver: Arc<dyn Approver>, deny: &[&str], dir: &Path) -> Agent {
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "bash", json!({"command": "ls"})),
        Script::text("done"),
    ]);
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode,
        workspace: dir.to_path_buf(),
        read_dirs: vec![],
        rules: RuleSet {
            deny: deny.iter().map(|r| r.to_string()).collect(),
            ..RuleSet::default()
        },
        sandbox_available: true,
        writes_need_approval: false,
    }));
    Agent::new(
        provider,
        ToolRegistry::new(vec![Arc::new(Shell)]),
        policy,
        approver,
        AgentConfig::new("mock/m1", "m1", "system prompt", dir.join(".spill")),
        ToolContext::new(dir).with_sandbox(Some(Arc::new(Dropped)), mode.fs_access()),
    )
}

fn output(events: &[AgentEvent]) -> (String, bool) {
    events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolCallFinished {
                output, is_error, ..
            } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .unwrap()
}

#[tokio::test]
async fn headless_the_command_is_blocked_and_does_not_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(Mode::Auto, Arc::new(NonInteractive), &[], dir.path());
    let (_, events) = run(&mut agent, "go").await;
    let (text, is_error) = output(&events);
    assert!(
        is_error && text.starts_with("blocked: the sandbox dropped"),
        "{text}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ActionBlocked { .. })),
        "a headless run exits 3"
    );
}

#[tokio::test]
async fn approved_it_runs_outside_the_sandbox_once_asked_once() {
    let dir = tempfile::tempdir().unwrap();
    let answer = Arc::new(Answer(
        ApprovalDecision::ApproveForSession,
        Mutex::default(),
    ));
    // Ask mode would ask about `ls` anyway: still one question.
    let mut agent = agent(Mode::Ask, answer.clone(), &[], dir.path());
    let (_, events) = run(&mut agent, "go").await;
    assert_eq!(output(&events), ("ran, unsandboxed=true".into(), false));
    let asked = answer.1.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].kind, ApprovalKind::RunUnsandboxed);
    assert_eq!(
        asked[0].reason,
        "the sandbox dropped to the basic tier; run `ls` without the sandbox?"
    );
}

#[tokio::test]
async fn declined_it_does_not_run() {
    let dir = tempfile::tempdir().unwrap();
    let answer = Arc::new(Answer(
        ApprovalDecision::Deny {
            feedback: Some("not now".into()),
        },
        Mutex::default(),
    ));
    let mut agent = agent(Mode::Auto, answer, &[], dir.path());
    let (_, events) = run(&mut agent, "go").await;
    assert_eq!(
        output(&events),
        (
            "the user declined to run it without the sandbox: not now".into(),
            true
        )
    );
}

#[tokio::test]
async fn read_only_commands_and_denied_ones_are_not_asked_about() {
    let dir = tempfile::tempdir().unwrap();
    let answer = Arc::new(Answer(ApprovalDecision::Approve, Mutex::default()));
    // Plan mode's read-only sandbox can still run it.
    let mut agent_ro = agent(Mode::Plan, answer.clone(), &[], dir.path());
    let (_, events) = run(&mut agent_ro, "go").await;
    assert_eq!(output(&events), ("ran, unsandboxed=false".into(), false));
    // A deny rule wins before anything is asked.
    let mut denied = agent(Mode::Auto, answer.clone(), &["bash:ls*"], dir.path());
    let (_, events) = run(&mut denied, "go").await;
    assert!(output(&events).0.starts_with("denied:"));
    assert!(answer.1.lock().unwrap().is_empty());
}
```

Every call to the guard's `begin` loses its `save_all` argument:

Run: `perl -pi -e 's/\.begin\((&[\w.]+), (?:true|false), /.begin($1, /' crates/harness-sandbox/src/guard/tests.rs crates/harness-sandbox/tests/git_guard.rs crates/harness-sandbox/src/watch.rs crates/harness-sandbox/src/linux/inotify.rs crates/harness-sandbox/src/linux/mod.rs`

The tests of a guard that saves only some files go, and those named after it are renamed. In `crates/harness-sandbox/tests/git_guard.rs`:

Replace (1 of 3):

```rust
}

#[test]
fn without_saving_everything_only_new_names_are_undone() {
    // The Linux full tier: read-only mounts stop changes to existing entries, so they are not
    // saved; new names still appear, because mounts cannot cover what does not exist.
    let env = env();
    let guard = env.session.begin(&env.ws, |_| {});
    std::fs::write(
        env.ws.join(".git/config"),
        "changed through a mount that was not there\n",
    )
    .unwrap();
    std::fs::write(env.ws.join(".git/commondir"), "/tmp/evil\n").unwrap();
    let report = guard.finish().expect("a report");
    assert_eq!(
        read(&env.ws.join(".git/config")),
        "changed through a mount that was not there\n"
    );
    assert!(gone(&env.ws.join(".git/commondir")));
    assert_eq!(read(&quarantined(&env, ".git/commondir")), "/tmp/evil\n");
    assert!(
        !report.message.contains(".git/config"),
        "{}",
        report.message
    );
}

#[test]
fn a_repointed_hooks_symlink_is_put_back_even_without_saving_everything() {
    // Read-only mounts cannot cover a symlink, so the full tier relies on the guard for it.
    let env = env();
    std::fs::rename(env.ws.join(".git/hooks"), env.ws.join("tracked-hooks")).unwrap();
```

with:

```rust
}

#[test]
fn a_repointed_hooks_symlink_is_put_back() {
    // Read-only mounts cannot cover a symlink, so the full tier relies on the guard for it.
    let env = env();
    std::fs::rename(env.ws.join(".git/hooks"), env.ws.join("tracked-hooks")).unwrap();
```

Replace (2 of 3):

```rust
}

#[test]
fn a_hard_linked_config_is_restored_even_without_saving_everything() {
    let env = env();
    std::fs::hard_link(env.ws.join(".git/config"), env.ws.join("alias")).unwrap();
    let guard = env.session.begin(&env.ws, |_| {});
```

with:

```rust
}

#[test]
fn a_config_changed_through_a_hard_link_is_restored() {
    let env = env();
    std::fs::hard_link(env.ws.join(".git/config"), env.ws.join("alias")).unwrap();
    let guard = env.session.begin(&env.ws, |_| {});
```

Replace (3 of 3):

```rust
}

#[test]
fn in_the_full_tier_changes_between_commands_are_not_restored() {
    // Survivors stay under the read-only mounts there; only new names are checked.
    let env = env();
    env.session.set_survivor_probe(Arc::new(|| true));
    assert_eq!(env.session.begin(&env.ws, |_| {}).finish(), None);
    evil_config(&env);
    std::fs::write(env.ws.join(".git/commondir"), "/tmp/evil\n").unwrap();
    let report = env
        .session
        .begin(&env.ws, |_| {})
        .finish()
        .expect("a report");
    assert!(
        !report.message.contains(".git/config"),
        "{}",
        report.message
    );
    assert!(report.message.contains("- .git/commondir: new; moved to "));
    assert_eq!(
        read(&env.ws.join(".git/config")),
        "[core]\n\tfsmonitor = /tmp/evil\n"
    );
}

#[test]
fn a_watcher_can_undo_changes_between_commands_while_survivors_live() {
    let env = env();
    env.session.set_survivor_probe(Arc::new(|| true));
```

with:

```rust
}

#[test]
fn a_watcher_can_undo_changes_between_commands_while_survivors_live() {
    let env = env();
    env.session.set_survivor_probe(Arc::new(|| true));
```

In `crates/harness-sandbox/src/guard/tests.rs`:

Replace:

```rust
}

#[test]
fn a_capped_restore_in_the_full_tier_is_done_at_the_next_command() {
    let env = env();
    let hooks = env.ws.join(".git/hooks");
    std::fs::rename(&hooks, env.ws.join("tracked-hooks")).unwrap();
```

with:

```rust
}

#[test]
fn a_capped_restore_of_a_symlink_is_done_at_the_next_command() {
    let env = env();
    let hooks = env.ws.join(".git/hooks");
    std::fs::rename(&hooks, env.ws.join("tracked-hooks")).unwrap();
```

The Linux sandbox's answer, run in Linux CI. In `crates/harness-sandbox/src/linux/mod.rs`:

Replace:

```rust
        )
    }

    #[test]
    fn a_watcher_that_cannot_start_is_said_once_in_the_next_report() {
        let _serial = procs::serial();
```

with:

```rust
        )
    }

    // 2.13 final review M1: after a drop to the basic tier with "required", the agent asks to
    // run a writing command outside the sandbox, as a session that started so does.
    #[test]
    fn only_writing_commands_after_a_drop_with_required_protection_cannot_run() {
        let required = crate::SandboxSettings {
            require_full_git_protection: true,
            ..crate::SandboxSettings::default()
        };
        let dropped = LinuxSandbox::with_git_protection(
            required.clone(),
            GitProtection::Basic {
                reason: "the full tier's setup failed during the session: x".into(),
            },
        );
        let why = dropped.cannot_run(FsAccess::WorkspaceWrite).unwrap();
        assert!(why.contains("linux_git_protection = \"required\""), "{why}");
        assert!(why.contains("setup failed during the session: x"), "{why}");
        assert_eq!(dropped.cannot_run(FsAccess::ReadOnly), None);
        let full = LinuxSandbox::with_git_protection(required, GitProtection::Full);
        assert_eq!(full.cannot_run(FsAccess::WorkspaceWrite), None);
        assert_eq!(sandbox().cannot_run(FsAccess::WorkspaceWrite), None);
    }

    #[test]
    fn a_watcher_that_cannot_start_is_said_once_in_the_next_report() {
        let _serial = procs::serial();
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p harness-core --test unsandboxed`
Expected: FAIL to compile: ``method `cannot_run` is not a member of trait `CommandSandbox` ``.

Run: `cargo test -p harness-sandbox --test git_guard`
Expected: FAIL to compile: ``this method takes 3 arguments but 2 arguments were supplied`` (`begin`), 47 times.

- [ ] **Step 3: M1: ask before running outside a sandbox that cannot run the command**

In `crates/harness-core/src/tool.rs`:

Replace:

```rust
    fn git_protection(&self) -> GitProtection {
        GitProtection::Full
    }
}

/// Tools in a fixed order, so tool definitions are byte-identical across requests.
```

with:

```rust
    fn git_protection(&self) -> GitProtection {
        GitProtection::Full
    }
    /// Why this sandbox would refuse, now, a command with `access`, if it would: on Linux, once a
    /// session that requires full git-metadata protection dropped to the basic tier, a command
    /// that may write. The agent then asks whether to run the command outside the sandbox, as a
    /// session without one asks for every command. The default is `None`.
    fn cannot_run(&self, access: FsAccess) -> Option<String> {
        let _ = access;
        None
    }
}

/// Tools in a fixed order, so tool definitions are byte-identical across requests.
```

In `crates/harness-core/src/agent.rs`:

Replace (1 of 2):

```rust

        let action = tool.action(&args, &self.ctx);
        let mutating = self.is_mutating(&action);
        match self.policy.check(&action) {
            Decision::Allow => {}
            Decision::Deny(reason) => return ToolOutput::error(format!("denied: {reason}")),
            Decision::Ask(reason) => {
```

with:

```rust

        let action = tool.action(&args, &self.ctx);
        let mutating = self.is_mutating(&action);
        let decision = self.policy.check(&action);
        // A sandbox that can no longer run this command (Linux, git protection required, after a
        // drop to the basic tier) leaves one way to run it: outside the sandbox, if approved.
        if let (Decision::Allow | Decision::Ask(_), Action::Bash(command)) = (&decision, &action)
            && !self.ctx.unsandboxed
            && let Some(why) = self
                .ctx
                .sandbox
                .as_ref()
                .and_then(|sandbox| sandbox.cannot_run(self.ctx.access))
        {
            let command = command.clone();
            return self
                .run_outside_sandbox(call, &tool, args, &command, &why, mutating, events)
                .await;
        }
        match decision {
            Decision::Allow => {}
            Decision::Deny(reason) => return ToolOutput::error(format!("denied: {reason}")),
            Decision::Ask(reason) => {
```

Replace (2 of 2):

```rust
                .await;
        }
        output
    }

    /// A command failed inside the sandbox in a way that looks like a denial: ask whether to run
```

with:

```rust
                .await;
        }
        output
    }

    /// The sandbox cannot run `command` now, for the reason `why`: asks whether to run it outside
    /// the sandbox, once. Nobody to ask blocks it.
    #[allow(
        clippy::too_many_arguments,
        reason = "what executing the call knows, passed on"
    )]
    async fn run_outside_sandbox(
        &mut self,
        call: &ToolCall,
        tool: &Arc<dyn Tool>,
        args: Value,
        command: &str,
        why: &str,
        mutating: bool,
        events: &UnboundedSender<AgentEvent>,
    ) -> ToolOutput {
        let shown: String = command.chars().take(80).collect();
        let reason = format!("{why}; run `{shown}` without the sandbox?");
        let _ = events.send(AgentEvent::ApprovalNeeded {
            id: call.id.clone(),
            reason: reason.clone(),
        });
        let request = ApprovalRequest {
            call_id: call.id.clone(),
            tool: call.name.clone(),
            action: Action::Bash(command.to_string()),
            reason,
            kind: ApprovalKind::RunUnsandboxed,
        };
        match self.approver.decide(&request).await {
            ApprovalDecision::Approve | ApprovalDecision::ApproveForSession => {
                if mutating {
                    self.checkpoint(events).await;
                }
                let mut ctx = self.ctx.clone();
                ctx.unsandboxed = true;
                tool.run(args, &ctx).await
            }
            ApprovalDecision::Deny {
                feedback: Some(note),
            } => ToolOutput::error(format!(
                "the user declined to run it without the sandbox: {note}"
            )),
            ApprovalDecision::Deny { feedback: None } => {
                ToolOutput::error("the user declined to run it without the sandbox")
            }
            ApprovalDecision::Unavailable => {
                let blocked = format!(
                    "{why}, and no user is available to approve running the command without the sandbox"
                );
                let _ = events.send(AgentEvent::ActionBlocked {
                    id: call.id.clone(),
                    reason: blocked.clone(),
                });
                ToolOutput::error(format!("blocked: {blocked}"))
            }
        }
    }

    /// A command failed inside the sandbox in a way that looks like a denial: ask whether to run
```

Run: `cargo test -p harness-core`
Expected: PASS, including `headless_the_command_is_blocked_and_does_not_run` and `approved_it_runs_outside_the_sandbox_once_asked_once`.

- [ ] **Step 4: M4: the guard always saves every protected file**

In `crates/harness-sandbox/src/guard/snapshot.rs`:

Replace (1 of 18):

```rust

impl Snapshot {
    /// Records `roots` (below `tree`) and everything below them, without
    /// following symlinks. With `everything`, every entry is recorded and
    /// regular files' bytes are saved (up to the size limits). Without it,
    /// only what a read-only mount cannot protect is: symlinks, which cannot
    /// be mounted over, and regular files with more than one hard link,
    /// which can be written through another name.
    ///
    /// What `leave_out` picks is not recorded, and so counts as new in a
    /// directory recorded whole.
    pub(crate) fn take(
        tree: &Tree,
        roots: &[PathBuf],
        everything: bool,
        leave_out: impl Fn(&Path) -> bool,
    ) -> Snapshot {
        let mut snapshot = Snapshot::default();
        for root in roots {
            if let Ok((parent, name)) = tree.parent(root) {
                snapshot.record(&parent, &name, root, 0, everything, &leave_out);
            }
        }
        snapshot
```

with:

```rust

impl Snapshot {
    /// Records `roots` (below `tree`) and everything below them, without
    /// following symlinks: every entry is recorded and regular files' bytes
    /// are saved (up to the size limits).
    ///
    /// What `leave_out` picks is not recorded, and so counts as new in a
    /// directory recorded whole.
    pub(crate) fn take(
        tree: &Tree,
        roots: &[PathBuf],
        leave_out: impl Fn(&Path) -> bool,
    ) -> Snapshot {
        let mut snapshot = Snapshot::default();
        for root in roots {
            if let Ok((parent, name)) = tree.parent(root) {
                snapshot.record(&parent, &name, root, 0, &leave_out);
            }
        }
        snapshot
```

Replace (2 of 18):

```rust
        name: &OsStr,
        path: &Path,
        depth: usize,
        everything: bool,
        leave_out: &dyn Fn(&Path) -> bool,
    ) -> bool {
        if leave_out(path) {
```

with:

```rust
        name: &OsStr,
        path: &Path,
        depth: usize,
        leave_out: &dyn Fn(&Path) -> bool,
    ) -> bool {
        if leave_out(path) {
```

Replace (3 of 18):

```rust
            return false;
        };
        let node = match stat.kind {
            Kind::File if everything || stat.nlink > 1 => {
                let content = match self.save(parent, name, &stat) {
                    Some(bytes) => Content::Saved(bytes),
                    None => Content::Unsaved(Stamp::of(&stat)),
```

with:

```rust
            return false;
        };
        let node = match stat.kind {
            Kind::File => {
                let content = match self.save(parent, name, &stat) {
                    Some(bytes) => Content::Saved(bytes),
                    None => Content::Unsaved(Stamp::of(&stat)),
```

Replace (4 of 18):

```rust
                }
            }
            Kind::Dir => {
                return self.record_dir(parent, name, path, &stat, depth, everything, leave_out);
            }
            Kind::Symlink => match parent.read_link(name) {
                Ok(target) => Node::Symlink { target },
                Err(_) => return false,
            },
            Kind::Other if everything => Node::Other {
                dev: stat.dev,
                ino: stat.ino,
                birth: stat.birth,
            },
            Kind::File | Kind::Other => return false,
        };
        self.nodes.insert(path.to_path_buf(), node);
        true
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the walk's state, passed down as it goes"
    )]
    fn record_dir(
        &mut self,
        parent: &Dir,
```

with:

```rust
                }
            }
            Kind::Dir => {
                return self.record_dir(parent, name, path, &stat, depth, leave_out);
            }
            Kind::Symlink => match parent.read_link(name) {
                Ok(target) => Node::Symlink { target },
                Err(_) => return false,
            },
            Kind::Other => Node::Other {
                dev: stat.dev,
                ino: stat.ino,
                birth: stat.birth,
            },
        };
        self.nodes.insert(path.to_path_buf(), node);
        true
    }

    fn record_dir(
        &mut self,
        parent: &Dir,
```

Replace (5 of 18):

```rust
        path: &Path,
        stat: &Stat,
        depth: usize,
        everything: bool,
        leave_out: &dyn Fn(&Path) -> bool,
    ) -> bool {
        if everything {
            let node = Node::Dir {
                mode: stat.mode,
                whole: false,
            };
            self.nodes.insert(path.to_path_buf(), node);
        }
        let listed = parent.open_dir(name).and_then(|dir| {
            if dir.stat_self()?.same_entry(stat) {
                Ok((dir.entries()?, dir))
```

with:

```rust
        path: &Path,
        stat: &Stat,
        depth: usize,
        leave_out: &dyn Fn(&Path) -> bool,
    ) -> bool {
        let node = Node::Dir {
            mode: stat.mode,
            whole: false,
        };
        self.nodes.insert(path.to_path_buf(), node);
        let listed = parent.open_dir(name).and_then(|dir| {
            if dir.stat_self()?.same_entry(stat) {
                Ok((dir.entries()?, dir))
```

Replace (6 of 18):

```rust
                let mut whole = true;
                for child in names {
                    let child_path = path.join(&child);
                    whole &=
                        self.record(&dir, &child, &child_path, depth + 1, everything, leave_out);
                }
                whole
            }
```

with:

```rust
                let mut whole = true;
                for child in names {
                    let child_path = path.join(&child);
                    whole &= self.record(&dir, &child, &child_path, depth + 1, leave_out);
                }
                whole
            }
```

Replace (7 of 18):

```rust
        {
            *recorded = whole;
        }
        everything
    }

    /// The bytes of the regular file `name` in `parent`, when they fit.
```

with:

```rust
        {
            *recorded = whole;
        }
        true
    }

    /// The bytes of the regular file `name` in `parent`, when they fit.
```

Replace (8 of 18):

```rust
    #[test]
    fn nothing_changed_means_no_differences() {
        let (_d, tree, git) = gitdir();
        let snapshot = Snapshot::take(&tree, &roots(&git), true, |_| false);
        assert!(snapshot.differences(&tree).is_empty());
    }

    #[test]
    fn changes_deletions_and_additions_are_found_and_undone() {
        let (_d, tree, git) = gitdir();
        let snapshot = Snapshot::take(&tree, &roots(&git), true, |_| false);
        std::fs::write(git.join("config"), "[core]\n\thooksPath = /tmp\n").unwrap();
        std::fs::remove_file(git.join("hooks/pre-commit")).unwrap();
        std::fs::write(git.join("hooks/post-checkout"), "evil\n").unwrap();
```

with:

```rust
    #[test]
    fn nothing_changed_means_no_differences() {
        let (_d, tree, git) = gitdir();
        let snapshot = Snapshot::take(&tree, &roots(&git), |_| false);
        assert!(snapshot.differences(&tree).is_empty());
    }

    #[test]
    fn changes_deletions_and_additions_are_found_and_undone() {
        let (_d, tree, git) = gitdir();
        let snapshot = Snapshot::take(&tree, &roots(&git), |_| false);
        std::fs::write(git.join("config"), "[core]\n\thooksPath = /tmp\n").unwrap();
        std::fs::remove_file(git.join("hooks/pre-commit")).unwrap();
        std::fs::write(git.join("hooks/post-checkout"), "evil\n").unwrap();
```

Replace (9 of 18):

```rust
    #[test]
    fn a_directory_replaced_by_a_file_is_changed() {
        let (_d, tree, git) = gitdir();
        let snapshot = Snapshot::take(&tree, &roots(&git), true, |_| false);
        std::fs::remove_dir_all(git.join("hooks")).unwrap();
        std::fs::write(git.join("hooks"), "not a directory").unwrap();
        let found = snapshot.differences(&tree);
```

with:

```rust
    #[test]
    fn a_directory_replaced_by_a_file_is_changed() {
        let (_d, tree, git) = gitdir();
        let snapshot = Snapshot::take(&tree, &roots(&git), |_| false);
        std::fs::remove_dir_all(git.join("hooks")).unwrap();
        std::fs::write(git.join("hooks"), "not a directory").unwrap();
        let found = snapshot.differences(&tree);
```

Replace (10 of 18):

```rust
        let (_d, tree, git) = gitdir();
        let hook = git.join("hooks/pre-commit");
        std::fs::set_permissions(&hook, PermissionsExt::from_mode(0o644)).unwrap();
        let snapshot = Snapshot::take(&tree, &roots(&git), true, |_| false);
        std::fs::set_permissions(&hook, PermissionsExt::from_mode(0o755)).unwrap();
        assert_eq!(
            snapshot.differences(&tree),
```

with:

```rust
        let (_d, tree, git) = gitdir();
        let hook = git.join("hooks/pre-commit");
        std::fs::set_permissions(&hook, PermissionsExt::from_mode(0o644)).unwrap();
        let snapshot = Snapshot::take(&tree, &roots(&git), |_| false);
        std::fs::set_permissions(&hook, PermissionsExt::from_mode(0o755)).unwrap();
        assert_eq!(
            snapshot.differences(&tree),
```

Replace (11 of 18):

```rust
    }

    #[test]
    fn without_everything_only_hard_linked_files_and_symlinks_are_recorded() {
        let (_d, tree, git) = gitdir();
        std::fs::hard_link(git.join("config"), git.parent().unwrap().join("alias")).unwrap();
        symlink("../shared-hooks", git.join("hooks/link")).unwrap();
        let snapshot = Snapshot::take(&tree, &roots(&git), false, |_| false);
        std::fs::write(git.join("hooks/pre-commit"), "changed\n").unwrap();
        std::fs::write(git.join("hooks/new"), "added\n").unwrap();
        assert!(
            snapshot.differences(&tree).is_empty(),
            "other hooks are not recorded"
        );
        std::fs::remove_file(git.join("hooks/link")).unwrap();
        symlink("/tmp/evil", git.join("hooks/link")).unwrap();
        assert_eq!(
            snapshot.differences(&tree),
            vec![(git.join("hooks/link"), Difference::Changed)]
        );
        std::fs::remove_file(git.join("hooks/link")).unwrap();
        snapshot.restore(&tree, &git.join("hooks/link")).unwrap();
        std::fs::write(
            git.parent().unwrap().join("alias"),
            "[core]\n\tfsmonitor = x\n",
        )
        .unwrap();
        assert_eq!(
            snapshot.differences(&tree),
            vec![(git.join("config"), Difference::Changed)]
        );
        std::fs::remove_file(git.join("config")).unwrap();
        snapshot.restore(&tree, &git.join("config")).unwrap();
        assert_eq!(
            std::fs::read_to_string(git.join("config")).unwrap(),
            "[core]\n"
        );
    }

    #[test]
    fn a_large_file_is_compared_but_cannot_be_restored() {
        let (_d, tree, git) = gitdir();
        let big = vec![b'x'; (MAX_FILE_BYTES + 1) as usize];
        std::fs::write(git.join("hooks/big"), &big).unwrap();
        let snapshot = Snapshot::take(&tree, &roots(&git), true, |_| false);
        std::fs::write(git.join("hooks/big"), b"small").unwrap();
        assert_eq!(
            snapshot.differences(&tree),
```

with:

```rust
    }

    #[test]
    fn a_large_file_is_compared_but_cannot_be_restored() {
        let (_d, tree, git) = gitdir();
        let big = vec![b'x'; (MAX_FILE_BYTES + 1) as usize];
        std::fs::write(git.join("hooks/big"), &big).unwrap();
        let snapshot = Snapshot::take(&tree, &roots(&git), |_| false);
        std::fs::write(git.join("hooks/big"), b"small").unwrap();
        assert_eq!(
            snapshot.differences(&tree),
```

Replace (12 of 18):

```rust
    #[test]
    fn a_fifo_swapped_in_is_changed_and_never_blocks() {
        let (_d, tree, git) = gitdir();
        let snapshot = Snapshot::take(&tree, &roots(&git), true, |_| false);
        std::fs::remove_file(git.join("config")).unwrap();
        mkfifo(&git.join("config"));
        let found = bounded(move || snapshot.differences(&tree));
```

with:

```rust
    #[test]
    fn a_fifo_swapped_in_is_changed_and_never_blocks() {
        let (_d, tree, git) = gitdir();
        let snapshot = Snapshot::take(&tree, &roots(&git), |_| false);
        std::fs::remove_file(git.join("config")).unwrap();
        mkfifo(&git.join("config"));
        let found = bounded(move || snapshot.differences(&tree));
```

Replace (13 of 18):

```rust
    fn a_symlink_is_restored_with_its_target() {
        let (_d, tree, git) = gitdir();
        symlink("../shared-hooks", git.join("hooks/link")).unwrap();
        let snapshot = Snapshot::take(&tree, &roots(&git), true, |_| false);
        std::fs::remove_file(git.join("hooks/link")).unwrap();
        symlink("/tmp/evil", git.join("hooks/link")).unwrap();
        assert_eq!(
```

with:

```rust
    fn a_symlink_is_restored_with_its_target() {
        let (_d, tree, git) = gitdir();
        symlink("../shared-hooks", git.join("hooks/link")).unwrap();
        let snapshot = Snapshot::take(&tree, &roots(&git), |_| false);
        std::fs::remove_file(git.join("hooks/link")).unwrap();
        symlink("/tmp/evil", git.join("hooks/link")).unwrap();
        assert_eq!(
```

Replace (14 of 18):

```rust
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("pre-commit"), "outside\n").unwrap();
        std::fs::write(outside.join("post-checkout"), "outside hook\n").unwrap();
        let snapshot = Snapshot::take(&tree, &roots(&git), true, |_| false);
        std::fs::rename(git.join("hooks"), git.join("hooks-old")).unwrap();
        symlink(&outside, git.join("hooks")).unwrap();
        assert_eq!(
```

with:

```rust
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("pre-commit"), "outside\n").unwrap();
        std::fs::write(outside.join("post-checkout"), "outside hook\n").unwrap();
        let snapshot = Snapshot::take(&tree, &roots(&git), |_| false);
        std::fs::rename(git.join("hooks"), git.join("hooks-old")).unwrap();
        symlink(&outside, git.join("hooks")).unwrap();
        assert_eq!(
```

Replace (15 of 18):

```rust
    #[test]
    fn a_restore_never_replaces_what_is_there() {
        let (_d, tree, git) = gitdir();
        let snapshot = Snapshot::take(&tree, &roots(&git), true, |_| false);
        std::fs::write(git.join("config"), "[core]\n\tfsmonitor = x\n").unwrap();
        let err = snapshot.restore(&tree, &git.join("config")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
```

with:

```rust
    #[test]
    fn a_restore_never_replaces_what_is_there() {
        let (_d, tree, git) = gitdir();
        let snapshot = Snapshot::take(&tree, &roots(&git), |_| false);
        std::fs::write(git.join("config"), "[core]\n\tfsmonitor = x\n").unwrap();
        let err = snapshot.restore(&tree, &git.join("config")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
```

Replace (16 of 18):

```rust
        // A directory whose permissions are put back can be looked into
        // again, in the same walk.
        let (_d, tree, git) = gitdir();
        let snapshot = Snapshot::take(&tree, &roots(&git), true, |_| false);
        let hooks = git.join("hooks");
        std::fs::write(hooks.join("post-checkout"), "evil\n").unwrap();
        std::fs::set_permissions(&hooks, PermissionsExt::from_mode(0o000)).unwrap();
```

with:

```rust
        // A directory whose permissions are put back can be looked into
        // again, in the same walk.
        let (_d, tree, git) = gitdir();
        let snapshot = Snapshot::take(&tree, &roots(&git), |_| false);
        let hooks = git.join("hooks");
        std::fs::write(hooks.join("post-checkout"), "evil\n").unwrap();
        std::fs::set_permissions(&hooks, PermissionsExt::from_mode(0o000)).unwrap();
```

Replace (17 of 18):

```rust
    #[test]
    fn an_entry_made_reachable_again_is_looked_at_again() {
        let (_d, tree, git) = gitdir();
        let snapshot = Snapshot::take(&tree, &roots(&git), true, |_| false);
        std::fs::write(git.join("hooks/post-checkout"), "evil\n").unwrap();
        std::fs::set_permissions(&git, PermissionsExt::from_mode(0o000)).unwrap();
        let mut found = Vec::new();
```

with:

```rust
    #[test]
    fn an_entry_made_reachable_again_is_looked_at_again() {
        let (_d, tree, git) = gitdir();
        let snapshot = Snapshot::take(&tree, &roots(&git), |_| false);
        std::fs::write(git.join("hooks/post-checkout"), "evil\n").unwrap();
        std::fs::set_permissions(&git, PermissionsExt::from_mode(0o000)).unwrap();
        let mut found = Vec::new();
```

Replace (18 of 18):

```rust
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("file"), "deep\n").unwrap();
        let snapshot = Snapshot::take(&tree, &roots(&git), true, |_| false);
        assert!(snapshot.differences(&tree).is_empty());
    }
}
```

with:

```rust
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("file"), "deep\n").unwrap();
        let snapshot = Snapshot::take(&tree, &roots(&git), |_| false);
        assert!(snapshot.differences(&tree).is_empty());
    }
}
```

In `crates/harness-sandbox/src/guard/mod.rs`:

Replace (1 of 10):

```rust
//! - before it, moves to quarantine protected names that appeared in a known
//!   gitdir, or at the top of the workspace, since the previous command
//!   ended (a process that command left running may have planted them), and,
//!   when such processes were running and every protected file was saved
//!   (`save_all`), restores the protected files they changed
//!   ([`GuardSession::set_survivor_probe`]);
//! - indexes the workspace ([`discover`](crate::gitmeta::discover), with the
//!   ignore rules read once per session: [`GuardSession::prime`]) and records
//!   which protected names exist, and with `save_all` saves the protected
//!   files;
//! - while it runs (a watcher calls [`WatchHandle::check`]) and after it
//!   ends, moves to quarantine every new protected name, new gitdir and
//!   replaced `.git`, and restores changed protected files;
```

with:

```rust
//! - before it, moves to quarantine protected names that appeared in a known
//!   gitdir, or at the top of the workspace, since the previous command
//!   ended (a process that command left running may have planted them), and,
//!   when such processes were running, restores the protected files they
//!   changed ([`GuardSession::set_survivor_probe`]);
//! - indexes the workspace ([`discover`](crate::gitmeta::discover), with the
//!   ignore rules read once per session: [`GuardSession::prime`]), records
//!   which protected names exist, and saves the protected files;
//! - while it runs (a watcher calls [`WatchHandle::check`]) and after it
//!   ends, moves to quarantine every new protected name, new gitdir and
//!   replaced `.git`, and restores changed protected files;
```

Replace (2 of 10):

```rust

    /// Sets what says whether processes that sandboxed commands started are
    /// still running. When it says so as a command's guard finishes, or at
    /// any check after that, a guard that saved every protected file
    /// (`save_all`) restores the protected files those processes change
    /// before the next command begins (and whenever
    /// [`between_commands`](Self::between_commands)' handle checks).
    pub fn set_survivor_probe(&self, probe: SurvivorProbe) {
        *lock(&self.probe) = probe;
```

with:

```rust

    /// Sets what says whether processes that sandboxed commands started are
    /// still running. When it says so as a command's guard finishes, or at
    /// any check after that, the guard restores the protected files those
    /// processes change before the next command begins (and whenever
    /// [`between_commands`](Self::between_commands)' handle checks).
    pub fn set_survivor_probe(&self, probe: SurvivorProbe) {
        *lock(&self.probe) = probe;
```

Replace (3 of 10):

```rust
    /// Starts guarding one command in the canonical `workspace`: see the
    /// module docs. `placeholders` runs after the workspace is indexed and
    /// before the existing protected names are recorded (the Linux full tier
    /// creates empty `hooks/` directories there). With `save_all`, every
    /// protected file is saved so it can be restored (both Linux tiers);
    /// without it, only protected symlinks and files with more than one hard
    /// link are.
    pub fn begin(
        self: &Arc<Self>,
        workspace: &Path,
        save_all: bool,
        placeholders: impl FnOnce(&GitIndex),
    ) -> GitGuard {
        let rules = self.rules(workspace);
```

with:

```rust
    /// Starts guarding one command in the canonical `workspace`: see the
    /// module docs. `placeholders` runs after the workspace is indexed and
    /// before the existing protected names are recorded (the Linux full tier
    /// creates empty `hooks/` directories there). Every protected file is
    /// saved so it can be restored, in both Linux tiers.
    pub fn begin(
        self: &Arc<Self>,
        workspace: &Path,
        placeholders: impl FnOnce(&GitIndex),
    ) -> GitGuard {
        let rules = self.rules(workspace);
```

Replace (4 of 10):

```rust
            .collect();
        // Entries still to be moved are left out, so they count as new; what
        // is still to be restored or put back keeps its earlier version.
        let mut snapshot = Snapshot::take(&tree, &roots, save_all, unknown);
        if let Some(earlier) = &earlier {
            if baseline.is_some() {
                snapshot.adopt_all(earlier);
```

with:

```rust
            .collect();
        // Entries still to be moved are left out, so they count as new; what
        // is still to be restored or put back keeps its earlier version.
        let mut snapshot = Snapshot::take(&tree, &roots, unknown);
        if let Some(earlier) = &earlier {
            if baseline.is_some() {
                snapshot.adopt_all(earlier);
```

Replace (5 of 10):

```rust
        let mut state = State {
            workspace: workspace.to_path_buf(),
            tree,
            save_all,
            index,
            gitdirs,
            nested,
```

with:

```rust
        let mut state = State {
            workspace: workspace.to_path_buf(),
            tree,
            index,
            gitdirs,
            nested,
```

Replace (6 of 10):

```rust
struct State {
    workspace: PathBuf,
    tree: Tree,
    save_all: bool,
    index: GitIndex,
    /// The gitdirs checked: the ones indexed, and [`nested`](Self::nested).
    gitdirs: BTreeSet<PathBuf>,
```

with:

```rust
struct State {
    workspace: PathBuf,
    tree: Tree,
    index: GitIndex,
    /// The gitdirs checked: the ones indexed, and [`nested`](Self::nested).
    gitdirs: BTreeSet<PathBuf>,
```

Replace (7 of 10):

```rust
            candidates,
            existing,
            snapshot: std::mem::take(&mut self.snapshot),
            save_all: self.save_all,
            detached: self.detached.clone(),
            identities: std::mem::take(&mut self.identities),
            survivors,
```

with:

```rust
            candidates,
            existing,
            snapshot: std::mem::take(&mut self.snapshot),
            detached: self.detached.clone(),
            identities: std::mem::take(&mut self.identities),
            survivors,
```

Replace (8 of 10):

```rust
    gitdirs: BTreeSet<PathBuf>,
    candidates: BTreeSet<PathBuf>,
    existing: BTreeSet<PathBuf>,
    /// The snapshot taken before the command: every protected file with
    /// `save_all`, only protected symlinks and multiply linked files without.
    snapshot: Snapshot,
    /// Whether the snapshot holds every protected file (`save_all`).
    save_all: bool,
    /// Below these, the snapshot is not compared: see [`State::detached`].
    detached: BTreeSet<PathBuf>,
    /// What each `.git` entry, gitdir and link was, and is expected to be:
```

with:

```rust
    gitdirs: BTreeSet<PathBuf>,
    candidates: BTreeSet<PathBuf>,
    existing: BTreeSet<PathBuf>,
    /// The snapshot taken before the command: every protected file.
    snapshot: Snapshot,
    /// Below these, the snapshot is not compared: see [`State::detached`].
    detached: BTreeSet<PathBuf>,
    /// What each `.git` entry, gitdir and link was, and is expected to be:
```

Replace (9 of 10):

```rust
    /// Does what the checks left undone first; then, if processes the
    /// command left running may have changed things, puts back or moves a
    /// `.git` entry, gitdir or link they replaced; moves protected names
    /// planted since the command ended to quarantine; and, with `save_all`
    /// if processes may have changed things, or whenever restores were left
    /// undone, undoes the changes to the protected files and gitfiles.
    fn check(
        &mut self,
        tree: &Tree,
```

with:

```rust
    /// Does what the checks left undone first; then, if processes the
    /// command left running may have changed things, puts back or moves a
    /// `.git` entry, gitdir or link they replaced; moves protected names
    /// planted since the command ended to quarantine; and, if processes may
    /// have changed things, or whenever restores were left undone, undoes
    /// the changes to the protected files and gitfiles.
    fn check(
        &mut self,
        tree: &Tree,
```

Replace (10 of 10):

```rust
            .undone
            .values()
            .any(|undone| matches!(undone.todo, Todo::Restore));
        if (self.survivors && self.save_all) || restores_left {
            let detached = &self.detached;
            pass.undo(
                &self.snapshot,
```

with:

```rust
            .undone
            .values()
            .any(|undone| matches!(undone.todo, Todo::Restore));
        if self.survivors || restores_left {
            let detached = &self.detached;
            pass.undo(
                &self.snapshot,
```

And the Linux sandbox: `begin` without `save_all`, `cannot_run`, and the message of the refusal that remains for a drop between the check and the command. In `crates/harness-sandbox/src/linux/mod.rs`:

Replace (1 of 3):

```rust
        // `begin` asks the probe only when an earlier command left something
        // to check, so orphans are reaped here as well.
        procs::look_and_reap();
        // Every protected file is saved in the full tier too: a rename or an
        // unlink from outside the command's namespace detaches its bind
        // there, and a process an earlier command left keeps its own
        // namespace, where what appeared since has no mount.
        let save_all = true;
        // The plan is made from the guard's index, after the scan and before
        // the guard records which protected names exist, so the guard takes
        // the `hooks/` placeholders for existing ones.
        let mut planned = None;
        let guard = self.guards.begin(&workspace, save_all, |index| {
            if full {
                planned = mountplan::plan(&workspace, index);
            }
```

with:

```rust
        // `begin` asks the probe only when an earlier command left something
        // to check, so orphans are reaped here as well.
        procs::look_and_reap();
        // The guard saves every protected file in the full tier too: a rename
        // or an unlink from outside the command's namespace detaches its bind
        // there, and a process an earlier command left keeps its own
        // namespace, where what appeared since has no mount. The plan is made
        // from the guard's index, after the scan and before the guard records
        // which protected names exist, so the guard takes the `hooks/`
        // placeholders for existing ones.
        let mut planned = None;
        let guard = self.guards.begin(&workspace, |index| {
            if full {
                planned = mountplan::plan(&workspace, index);
            }
```

Replace (2 of 3):

```rust
            // this) is not lost, exactly as the `mounted_command` error path
            // below does for a setup failure.
            let message = format!(
                "git metadata protection is required (sandbox.linux_git_protection = \"required\"), but the full tier is unavailable: {reason}; restart harness to have every command ask first"
            );
            let report = self.after(workspace).finished(guard.finish());
            return Err(match report {
```

with:

```rust
            // this) is not lost, exactly as the `mounted_command` error path
            // below does for a setup failure.
            let message = format!(
                "git metadata protection is required (sandbox.linux_git_protection = \"required\"), but the full tier is unavailable: {reason}; the command did not run"
            );
            let report = self.after(workspace).finished(guard.finish());
            return Err(match report {
```

Replace (3 of 3):

```rust

    fn git_protection(&self) -> GitProtection {
        lock(&self.tier).clone()
    }

    /// As harness exits: stops the watchers between commands, ends the
```

with:

```rust

    fn git_protection(&self) -> GitProtection {
        lock(&self.tier).clone()
    }

    /// A command that may write, once the session dropped to the basic tier while full git
    /// protection is required: the agent asks to run it outside the sandbox instead, as
    /// `prepare` would refuse it.
    fn cannot_run(&self, access: FsAccess) -> Option<String> {
        if access != FsAccess::WorkspaceWrite || !self.settings.require_full_git_protection {
            return None;
        }
        match self.git_protection() {
            GitProtection::Basic { reason } => Some(format!(
                "git metadata protection is required (sandbox.linux_git_protection = \"required\"), but the sandbox dropped to the basic tier ({reason})"
            )),
            GitProtection::Full => None,
        }
    }

    /// As harness exits: stops the watchers between commands, ends the
```

- [ ] **Step 5: The README**

In `README.md`:

Replace (1 of 5):

```markdown

A terminal-first, open-source coding agent written in Rust, built for **hybrid development**: mix local models with frontier models so you get work done while saving subscription usage.

> **Status: early development.** Milestone 1, phases P1 (foundation), P2 (safety) and P3 (memory) are complete: a headless `harness ask` that runs multi-step coding tasks against any OpenAI-compatible model in a sandboxed environment, with project instructions, slash commands, saved sessions and checkpoints. More providers and the interactive terminal UI are in progress. Not ready for daily use yet.

## What works today

- `harness ask "<prompt>"`: runs one task to completion with six tools (`read`, `write`, `edit`, `bash`, `grep`, `glob`). Piped stdin is appended to the prompt; `--json` streams every event as NDJSON.
- `harness models`: lists models from local servers (Ollama, LM Studio, llama.cpp) and configured providers.
- Providers: any OpenAI-compatible endpoint. Built in: `ollama`, `lmstudio`, `llamacpp`, `openrouter` (`OPENROUTER_API_KEY`).
```

with:

```markdown

A terminal-first, open-source coding agent written in Rust, built for **hybrid development**: mix local models with frontier models so you get work done while saving subscription usage.

> **Status: early development.** Milestone 1, phases P1 (foundation), P2 (safety) and P3 (memory) are complete: a headless `harness ask` that runs multi-step coding tasks against any OpenAI-compatible model in a sandboxed environment, with project instructions, slash commands, saved sessions and checkpoints. The interactive terminal UI's first half (P5a) is in: approvals, plan mode, steering and notifications. More providers and the UI's pickers are in progress. Not ready for daily use yet.

## What works today

- `harness` on its own: an interactive session in your terminal. The conversation goes into the terminal's own scrollback (tmux, search and copy work as usual); only the input and what is running are redrawn at the bottom. Replies render as Markdown with highlighted code; file changes show as diffs. Approvals are asked there: `y` once, `a` for the session, `n` with a reason for the model, `Esc` to stop the turn. Enter while a turn runs queues the message for the next turn; Ctrl+S gives it to the model with the next tool results. Shift+Tab cycles `plan`, `ask` and `auto`; in `plan` mode a finished plan offers Build, Edit (in `$EDITOR`) or Keep planning. `/` completes commands and `@` completes file paths; pastes over 10 lines collapse to `[Pasted text #n, N lines]` (Ctrl+O expands one) and are sent whole; `/context` and `/usage` show where the tokens go. A dim line after each turn shows the model, time to first token, tokens per second and the prompt-cache hit rate, and a turn over 10 seconds, or an approval, sends a desktop notification (OSC 9) and a bell. Esc interrupts, Ctrl+C twice exits. The first time a workspace's project settings would widen what the agent may do, harness lists them and asks whether to trust them.
- `harness ask "<prompt>"`: runs one task to completion with six tools (`read`, `write`, `edit`, `bash`, `grep`, `glob`). Piped stdin is appended to the prompt; `--json` streams every event as NDJSON.
- `harness models`: lists models from local servers (Ollama, LM Studio, llama.cpp) and configured providers.
- Providers: any OpenAI-compatible endpoint. Built in: `ollama`, `lmstudio`, `llamacpp`, `openrouter` (`OPENROUTER_API_KEY`).
```

Replace (2 of 5):

```markdown
- Exit codes for scripting: `0` success, `1` runtime error, `2` invalid usage or no model, `3` an action was blocked for lack of approval, `130` interrupted.
- Project instructions: `AGENTS.md` (or `CLAUDE.md` where a directory has no `AGENTS.md`) from `~/.config/harness/`, the repository root, and each directory down to the working directory, with `@path` import lines. They go into a system prompt that stays the same for the whole run, so model servers can reuse their prompt caches.
- Slash commands in `harness ask`: Markdown commands from `.harness/commands`, `.claude/commands` or `.opencode/commands` in the project, and from `~/.config/harness/commands` and `~/.claude/commands`, so OpenSpec's `/opsx:*` commands work as they are. `$ARGUMENTS`, `$1`…`$9`, `@file` and `` !`command` `` are filled in; a shell command gets the arguments as its parameters (`"$1"`, `"$ARGUMENTS"`), never written into its text, and shell commands go through the same approvals and sandbox as the `bash` tool. `harness ask "/init"` drafts an `AGENTS.md`.
- Sessions: every run is saved under `~/.local/share/harness/sessions/`. `harness -c ask "..."` continues the project's most recent session, `harness --resume` lists them, and `harness --resume <id> ask "..."` continues one.
- Checkpoints: before a turn first changes anything, the workspace is snapshotted into a separate git repository in harness's data directory; your own repository, index and history are never touched. Rewinding to a checkpoint arrives with the terminal UI.
- Compaction: when a conversation nears the context window, or a provider says a request is too long, older messages are replaced by a summary the model writes, and the summary is shown.

```

with:

```markdown
- Exit codes for scripting: `0` success, `1` runtime error, `2` invalid usage or no model, `3` an action was blocked for lack of approval, `130` interrupted.
- Project instructions: `AGENTS.md` (or `CLAUDE.md` where a directory has no `AGENTS.md`) from `~/.config/harness/`, the repository root, and each directory down to the working directory, with `@path` import lines. They go into a system prompt that stays the same for the whole run, so model servers can reuse their prompt caches.
- Slash commands in `harness ask`: Markdown commands from `.harness/commands`, `.claude/commands` or `.opencode/commands` in the project, and from `~/.config/harness/commands` and `~/.claude/commands`, so OpenSpec's `/opsx:*` commands work as they are. `$ARGUMENTS`, `$1`…`$9`, `@file` and `` !`command` `` are filled in; a shell command gets the arguments as its parameters (`"$1"`, `"$ARGUMENTS"`), never written into its text, and shell commands go through the same approvals and sandbox as the `bash` tool. `harness ask "/init"` drafts an `AGENTS.md`.
- Sessions: every run is saved under `~/.local/share/harness/sessions/`. `harness -c` (or `harness -c ask "..."`) continues the project's most recent session, `harness --resume` lists them, and `harness --resume <id>` continues one.
- Checkpoints: before a turn first changes anything, the workspace is snapshotted into a separate git repository in harness's data directory; your own repository, index and history are never touched. Rewinding to a checkpoint arrives with the terminal UI.
- Compaction: when a conversation nears the context window, or a provider says a request is too long, older messages are replaced by a summary the model writes, and the summary is shown.

```

Replace (3 of 5):

````markdown
```sh
cargo build --release -p harness-cli
./target/release/harness models
git diff | ./target/release/harness --model ollama/qwen3:14b ask "summarize this diff"
```

````

with:

````markdown
```sh
cargo build --release -p harness-cli
./target/release/harness models
./target/release/harness --model ollama/qwen3:14b
git diff | ./target/release/harness --model ollama/qwen3:14b ask "summarize this diff"
```

````

Replace (4 of 5):

````markdown
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
````

with:

````markdown
[compaction]
threshold_percent = 80    # summarize at this share of the context window
keep_recent_percent = 20  # keep this share of recent messages as they are

[notifications]
desktop = true  # OSC 9 when a long turn ends or an approval waits
bell = true
```

Project-level `.harness/config.toml` settings that widen what the agent may do (allow rules, `read_dirs`, model, providers, sandbox settings, a `mode` wider than your global or default mode, a `max_steps` above your global limit) only apply after `harness trust`, and so does a `[compaction] threshold_percent` below 50. The same trust lets a project's command files choose their own `model`; a repository with command files and no project settings can be trusted too.

## Known limitations

- **Linux git metadata without user namespaces (the basic tier):** where unprivileged user namespaces are blocked (stock Ubuntu 24.04 and later, Docker's default profile, GitHub's Ubuntu runners), harness protects git metadata only after the fact. After each sandboxed command it moves planted hooks, config and repositories to a quarantine directory in harness's data directory (`~/.local/share/harness/quarantine/` by default) and restores changed files, so a git process running outside the sandbox at that moment, such as an editor's status poll, could read a planted file first. A change you make yourself to `.git/config` or hooks while a command runs is undone too and kept in the quarantine. Only a kernel or host refusal drops a session to this tier; a one-off race only blocks that one command. `harness sandbox doctor` shows the one-time fix (an AppArmor profile for the harness binary, or a sysctl); `sandbox.linux_git_protection = "required"` makes harness ask before every command that could write instead, also once a session drops to this tier while it runs.
- **Linux git metadata in either tier:** mounts and checks cannot cover a name that does not exist yet. A new `commondir` in a gitdir, a top-level `HEAD` or `.harness/` is moved to the quarantine milliseconds after it appears rather than refused, a new nested repository only after the command ends, and one inside a git-ignored directory not at all. A repository or gitdir you add between commands can also be changed by a process an earlier command left running, before the next command's guard snapshots it; it's left alone as your own, since a repository created between commands is taken for your own clone. While a command runs, the watcher that catches a change undoes it but does not stop the command that made it.
- **The full tier also restores.** Renaming a file from outside its mount namespace, as git does whenever it rewrites `.git/config`, detaches the read-only bind on it in every other namespace, so a command running while the user runs git could otherwise write the new file. So the full tier snapshots and restores protected files the same way the basic tier does; a user's own `.git/config` edit made while a command runs is undone and kept in the quarantine too.
- **The quarantine** lives at `$XDG_DATA_HOME/harness/quarantine/<timestamp>/` and nothing there is ever deleted. A quarantined repository has every `.git` renamed to `dot-git` and every `HEAD` to `HEAD.quarantined`, so git refuses to treat it as one; rename it back in a copy to inspect it. A second version of the same path quarantined within one command is stored as `<name>.<n>`. If the quarantine itself can't be used, an entry is renamed in place as `<name>.harness-quarantine-<n>` instead; an entry harness can't move at all is reported, retried before each later command, and forgotten on restart.
````

Replace (5 of 5):

```markdown
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
```

with:

```markdown
- **Checkpoints** cover the working directory, not files over 10 MB, git-ignored files, `node_modules`, `target`, `.harness/` or a top-level `HEAD`, or what is inside nested repositories; a rewind leaves those alone, including a file that was ignored or too large when the checkpoint was taken. In a subdirectory of a repository, the repository's own ignore rules apply, unless they ignore that subdirectory itself (then only its own `.gitignore` files do); your global git excludes file does not. A checkpoint is restored only in the directory it was taken in, so a session continued from another directory can rewind its conversation but not those files. git stores only whether a file is executable: a restored file gets your umask's permissions, except files only you could read, which get theirs back. Checkpoints are off, with a warning, when harness's data directory is inside the workspace or a directory sandboxed commands can write to (a temp directory, or a `writable_roots` entry), and in a workspace whose first snapshot takes longer than 5 seconds. They need git 2.26 or later.
- **Command files run with their own settings.** A command file's `allowed-tools` pre-approve the commands it names (never beyond deny rules, destructive-command confirmation or the sandbox). Its `model` answers its invocations if the file is your own (`~/.config/harness/commands`, `~/.claude/commands`); a project's command file chooses the model only once `harness trust` has trusted the directory the command files come from, the repository root (run it there, not in a subdirectory); otherwise a note says the session's model answers instead. In a command file's `` !`…` `` commands, the arguments are shell parameters set before the command runs: write `"$1"` or `"$ARGUMENTS"` in double quotes, as in any script (an unquoted `$1` is split and globbed, and `'$1'` is the text `$1`). A command that uses the arguments with a construct where bash may evaluate a value as code or arithmetic (`$((…))`, `$[…]`, `let`, `declare`, subscripts, `=(…)`, `eval`, `trap`, `read`, `printf -v`, `source`, `.`, `${!…}`, `${…@P}`, `compgen`, `complete`, `enable`, and a few more) is not run, with a warning. That list defends ordinary command bodies; one that deliberately hands an argument to something that runs it later, such as `PS4`, `PROMPT_COMMAND` or `BASH_ENV`, is the command author's responsibility, as in any script. Read command files from repositories you did not write before running them.
- **Instruction files and command files are read when a run starts**; changes apply to the next run.
- **The interactive terminal is new.** Shift+Enter inserts a new line only in terminals that report it (those with the kitty keyboard protocol: kitty, WezTerm, Ghostty, foot, recent iTerm2); Alt+Enter, Ctrl+J, or a `\` before Enter work everywhere. Desktop notifications need a terminal that shows OSC 9 (iTerm2, WezTerm, kitty, Ghostty, Windows Terminal); inside tmux only the bell gets through unless passthrough is on. A long diff scrolls inside the approval prompt rather than in a full-screen view. A mode chosen with Shift+Tab during a turn applies when the turn ends. `/model`, `/mode`, `/new`, `/resume`, `/rewind`, `/compact` and `/login` say they are not available yet; the session and model pickers, the rewind list and first-run model choice come with the rest of the terminal UI. Code highlighting uses a dark theme. Input history is this session's (and a resumed session's) messages.

## Roadmap

| Milestone | Scope |
|---|---|
| M1 Core agent | Phases P1 foundation, P2 safety and P3 memory (AGENTS.md, slash commands, sessions, checkpoints, compaction) done, and the first half of P5, the terminal UI; P4 providers (ChatGPT sign-in, Anthropic, model profiles), then the rest of P5 (rewind picker, `/compact`, `/resume`, model and session pickers) |
| M2 Routing | Model roles, boundary-based switching, usage ledger and "$ saved", verification gates |
| M3 Agents | Subagents, delegation to Claude Code and Codex, parallel agents in worktrees |
| M4 Ecosystem | Hooks, MCP, Agent Skills, ACP server |
```

- [ ] **Step 6: Run the tests, and lint for macOS and Linux**

Run: `cargo test --workspace`
Expected: PASS (1,107 tests on macOS).

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

Lint `harness-sandbox` for both Linux targets with the lint probe from 2.13's plan (`target/linux-lint.sh`, which copies the crate next to a stand-in `harness-core` holding `tool.rs`'s sandbox types; `cannot_run` comes along with them).
Expected: `linux lint ok (x86_64-unknown-linux-gnu)` and `linux lint ok (aarch64-unknown-linux-gnu)`.

- [ ] **Step 7: Commit**

```bash
git add README.md crates/harness-core crates/harness-sandbox
git commit -F - <<'EOF'
fix(sandbox): ask before running outside a dropped sandbox; drop save_all

Two follow-ups from the 2.13 final review. M1: once a Linux session
that requires full git protection drops to the basic tier, the sandbox
says it cannot run a command that may write (CommandSandbox::
cannot_run), and the agent asks to run it outside the sandbox, as a
session that starts in the basic tier does; a headless run counts it
as blocked (exit 3) rather than as a plain tool error (exit 0). M4:
both tiers save every protected file, so the guard's save_all
parameter, the snapshot's partial mode and the tests of a
configuration that cannot occur go. The README describes the
interactive terminal.

<trailer lines from the controller>
EOF
```

---

## Roadmap

| Phase | What builds on this |
|---|---|
| P4 Providers | Replaces `start::context_window` with the window from model profiles and local servers (see "Before you start"); `/context`'s note says where the window came from; the interactive session redacts what it prints and writes `--debug`'s log |
| P5, the rest (after P4) | 5.7: the `/rewind` picker and Esc Esc on `rewind_points`, `rewind`, `undo_rewind`; `/compact [focus]` on `Agent::compact`; `/resume`, `/new` and the session picker; `/mode` on `Agent::set_mode`; `/model`, the model picker and first-run model choice; a full-screen view for pickers and long diffs; `/login` |
| M2 | The Build step can take a builder model; `Agent::approved_plan` gives it the plan |

## Plan Completion Checklist

- [ ] `cargo test --workspace` is green on macOS, and the pull request's CI is green on every job (`macos-latest` and the `ubuntu-24.04` jobs, which run Task 14's Linux test).
- [ ] At a real terminal (iTerm2 or Terminal.app, and one Linux terminal), with a local model: `harness` opens the session; a reply streams and lands in scrollback; Shift+Tab reaches `plan`, and Build switches back; an approval in `ask` mode shows a diff; Ctrl+S steers a running `sleep`; a 50-line paste collapses; the terminal is as it was after Ctrl+C twice (`stty -a` shows `ixon` again).
- [ ] In tmux, the same session's scrollback keeps every finished message.
- [ ] Tick 5.1 to 5.6 and 5.8 in `tasks.md` (the controller), noting that 5.7 and the P4-dependent parts remain.
