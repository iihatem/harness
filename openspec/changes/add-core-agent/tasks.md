# Tasks

M1 is delivered in five phases, each with its own Superpowers implementation plan in `docs/superpowers/plans/`. A phase's plan is written when the previous phase is done.

## 1. P1 Foundation (`docs/superpowers/plans/2026-09-24-m1-p1-foundation.md`)

- [x] 1.1 Workspace scaffold, licences, and CI; verify `cargo test -p harness-cli --test cli_smoke` passes
- [x] 1.2 Core message, event, provider, and permission types; verify `cargo test -p harness-core --test types`
- [x] 1.3 XDG paths and layered config that ignores widening project settings; verify `cargo test -p harness-config`
- [x] 1.4 Symlink-aware path resolution and baseline permission policy; verify `cargo test -p harness-core --test permission`
- [x] 1.5 Tool trait, read tracking, and large-output spilling; verify `cargo test -p harness-core --test tool`
- [x] 1.6 `read`, `write`, `edit` tools; verify `cargo test -p harness-tools --test fs_tools`
- [x] 1.7 `grep`, `glob` tools; verify `cargo test -p harness-tools --test search_tools`
- [x] 1.8 `bash` tool and builtin registry; verify `cargo test -p harness-tools --test bash_tool --test registry`
- [x] 1.9 OpenAI Chat Completions adapter; verify `cargo test -p harness-providers --test openai_chat_parser --test openai_chat_http`
- [x] 1.10 Local discovery and model-id resolution; verify `cargo test -p harness-providers --test discovery --test registry`
- [x] 1.11 Agent loop with validation, approvals, and spilling; verify `cargo test -p harness-core --test agent`
- [x] 1.12 Retries and interruption; verify `cargo test -p harness-core --test resilience`
- [x] 1.13 `harness ask` and `harness models`; verify `cargo test -p harness-cli --test ask_e2e` and `cargo deny check`

## 2. P2 Safety (plan written after P1)

- [ ] 2.1 Rule engine with `allow`/`deny`/`confirm` patterns and compound-command decomposition; verify the permissions-sandbox "Allow and deny rules" and "Compound shell commands" scenarios as tests
- [ ] 2.2 Destructive-command confirmation, reads-outside-workspace approval, and approve-for-session; verify the matching permissions-sandbox scenarios as tests
- [ ] 2.3 macOS Seatbelt sandbox for `bash` and command shell expansion; verify sandbox escape tests (write outside, network) on macOS CI
- [ ] 2.4 Linux Landlock + seccomp sandbox (bubblewrap when installed) and the no-sandbox fallback; verify sandbox escape tests on Ubuntu CI
- [ ] 2.5 Workspace trust for widening project settings; verify the configuration "Widening project settings require workspace trust" scenarios as tests

## 3. P3 Memory (plan written after P2)

- [ ] 3.1 `AGENTS.md`/`CLAUDE.md` discovery with confined `@` imports; verify the project-context discovery and import scenarios as tests
- [ ] 3.2 Cache-stable prompt assembly with session-start environment capture and oversize warning; verify byte-identical prefixes across turns in a test
- [ ] 3.3 Slash commands: built-in registry, Markdown discovery and namespacing, frontmatter, placeholders, headless use; verify `/opsx:propose` expands from `.claude/commands` in an e2e test
- [ ] 3.4 Sessions: incremental JSONL tree, resume (`-c`, `--resume`), truncated-file tolerance; verify the sessions scenarios as tests
- [ ] 3.5 Checkpoints: shadow-repository snapshots, rewind of code/conversation/both, undo last rewind, degradation; verify the checkpoints scenarios as tests
- [ ] 3.6 Compaction (automatic, `/compact`, overflow retry) and `/init`; verify the compaction scenarios with the mock provider

## 4. P4 Providers (plan written after P3)

- [ ] 4.1 OpenAI Responses adapter; verify with recorded SSE fixtures
- [ ] 4.2 Anthropic Messages adapter; verify with recorded SSE fixtures
- [ ] 4.3 Credential store (keychain, `0600` fallback), account profiles, `auth add`/`auth use`/`logout`; verify the provider-auth storage and profile scenarios as tests
- [ ] 4.4 ChatGPT sign-in (browser PKCE, device code, refresh on 401) and the Claude-credential prohibition; verify against a mock OAuth server
- [ ] 4.5 Model profiles and effective-context detection with warnings; verify the model-providers profile and context scenarios as tests
- [ ] 4.6 Text tool-call recovery and truncation handling; verify the matching model-providers scenarios as tests
- [ ] 4.7 Secret-redaction audit across logs, sessions, tool output, and NDJSON; verify with a canary-key test

## 5. P5 Terminal UI (plan written after P4)

- [ ] 5.1 Inline renderer with native scrollback, Markdown and diff rendering, `NO_COLOR`; verify with ratatui `TestBackend` snapshots
- [ ] 5.2 Input editor: history, multi-line, collapsed pastes, `/` and `@` completion; verify with snapshot and unit tests
- [ ] 5.3 Status line, per-turn stats, `/context`, and `/usage`; verify with snapshot tests
- [ ] 5.4 Interactive approvals with diffs, re-run-unsandboxed offer, and Shift+Tab mode cycling; verify with scripted-input tests
- [ ] 5.5 Steering (queued vs send-now input) in the runtime and UI; verify the agent-runtime steering scenarios as tests
- [ ] 5.6 Plan mode flow (Build / Edit in `$EDITOR` / Keep planning); verify the plan-mode scenarios as tests
- [ ] 5.7 `/rewind` picker, model and session pickers, first-run model choice; verify with scripted-input tests
- [ ] 5.8 Desktop notifications (OSC 9 + bell); verify emitted escape sequences in a test

## 6. M1 acceptance

- [ ] 6.1 Walk through the "M1 is done when" criteria from the design review (local multi-step task in `auto` mode sandboxed, ChatGPT sign-in with mid-session `/model` switch, `/opsx:propose` working, sandbox tests green on both OSes, `harness ask --json` with documented exit codes)
