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

## 2. P2 Safety (`docs/superpowers/plans/2026-09-26-m1-p2-safety.md`)

- [ ] 2.1 Spec updates for P2 decisions; verify `openspec validate add-core-agent --strict`
- [ ] 2.2 `harness-shell` command analysis; verify `cargo test -p harness-shell` (bypass table)
- [ ] 2.3 `[permissions]`/`[sandbox]` config, trust store, fingerprints; verify `cargo test -p harness-config`
- [ ] 2.4 `harness trust`; verify `cargo test -p harness-cli --test trust_e2e`
- [ ] 2.5 PermissionEngine (modes × rules × sandbox availability, session approvals); verify `cargo test -p harness-core --test engine`
- [ ] 2.6 `harness-sandbox` API, detection, denial heuristic, macOS Seatbelt; verify `cargo test -p harness-sandbox` on macOS
- [ ] 2.7 Linux Landlock + seccomp; verify `cargo test -p harness-sandbox` on ubuntu-24.04 CI
- [ ] 2.8 `bash` tool: bash without startup files, sandbox wrapping, denial flag; verify `cargo test -p harness-tools --test bash_tool`
- [ ] 2.9 Agent: approve-for-session and sandbox-denial re-run; verify `cargo test -p harness-core --test agent`
- [ ] 2.10 CLI wiring and end-to-end sandbox behaviour; verify `cargo test -p harness-cli --test sandbox_e2e`
- [ ] 2.11 CI pinned to ubuntu-24.04, README safety section; verify CI green on the P2 pull request
- [ ] 2.12 (deferred past M1) macOS kernel-log denial correlation
- [ ] 2.13 Linux git-metadata protection (required for M1): a mount-namespace/bubblewrap backend with read-only binds over `.git` config, hooks and commondir, `.harness/` and a top-level `HEAD`, used when available, with Landlock + seccomp as the fallback; verify with Linux integration tests in CI
- [ ] 2.14 P2 follow-ups from the final review: here-document tracking in the fallback scan must not turn deny matches into prompts; a quote-aware heredoc pre-screen so quoted `<<` patterns stay decomposable; escape config parse errors before printing; the write tool asks before editing dotfiles when the workspace is `$HOME`; fix the design.md wording on nested symlinked gitdirs

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
