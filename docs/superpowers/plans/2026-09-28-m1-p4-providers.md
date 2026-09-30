# M1 · P4 Providers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give harness the providers M1 promises. OpenAI API keys go through a new OpenAI Responses adapter and Anthropic API keys through an Anthropic Messages adapter. Keys are stored in the OS keychain (or a `0600` file) under account profiles, managed with `harness auth add`, `harness auth use` and `harness logout`. `harness login chatgpt` signs in with a ChatGPT account in the browser or with a device code, and tokens refresh on their own; Claude subscription credentials are refused. Model profiles give each model its context window and request settings, and local servers are asked what context they really run with. Tool calls a local model writes as text are recovered, and a reply cut off at the output limit never runs a partial tool call. Secrets are kept out of everything harness writes.

**Architecture:** `harness-providers` gains two adapters (`openai_responses`, `anthropic_messages`) on a shared server-sent-events reader (`sse`); the credential store (`credentials`: keychain through `keyring-core`, file fallback, account profiles); ChatGPT sign-in behind the default-on `chatgpt-login` feature (`chatgpt::oauth` for the flows, `chatgpt::auth` for the signed-in account and its refreshes); model profiles (`profiles`); and effective-context detection (`window`). `harness-core` gains per-request options (`RequestOptions`), text tool-call recovery (`textcalls`), cut-off handling in the agent loop, a `Redactor` that the session file and tool-output files go through, and quota errors that are not retried. `harness-config` reads `[profiles."<glob>"]`. The CLI adds `auth add`, `auth use`, `login`, `logout` and `--debug`, resolves each model's profile and window, and redacts what it prints.

**Tech Stack:** Rust 1.98, edition 2024; `reqwest` with `eventsource-stream` for streaming; `keyring-core` 1.0 with `apple-native-keyring-store` (macOS) and `zbus-secret-service-keyring-store` (Linux) for the keychain; `base64` and `sha2` for PKCE and JWT claims; `globset` for profile keys; `nix` (`term`) to read a key without echo; tests with `wiremock` (the providers, the OAuth server, local servers), `tempfile`, `assert_cmd`, and `keyring-core`'s mock store.

**Spec:** `openspec/changes/add-core-agent/` (binding): `design.md` D2, D3, D9, D10 and D14; `specs/model-providers` and `specs/provider-auth`; and the parts of `specs/configuration` (widening project settings), `specs/cli-interface` (management subcommands, `--debug`) and `specs/agent-runtime` (non-retryable provider errors, the event stream) that P4 touches. Task 1 writes this plan's refinements (below) into `design.md`, four of the specs and `tasks.md`.

## How this plan was checked

Every task was built in order, as its own commit, in a scratch clone of this repository: `/private/tmp/claude-501/-Users-mac-Desktop-dev-AI-harness/78b07910-5be3-48d0-b7e8-1da36ddc200d/scratchpad/p4-seq`, branch `p4`, on top of `f2cc9fb` (P3 merged). The commits are `T1` to `T10`, one per task: `T1` 2bf7c0b, `T2` a9d712d, `T3` adf8de6, `T4` f2e4148, `T5` f504ce1, `T6` 1186b2c, `T7` 62dbac3, `T8` f6d847e, `T9` 13e7e54, `T10` ea5d9d4. Implementers may copy from them (`git -C <clone> show <commit>:<path>`), checking what they copy against this plan. Each commit was checked on its own, on macOS:

- `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings`; from Task 5 on also `cargo clippy -p harness-providers --no-default-features --all-targets -- -D warnings`, and from Task 6 the same for `harness-cli`, so a build without ChatGPT sign-in stays clean;
- the tests the task names, and `Cargo.lock` current (`cargo metadata --locked`); after Task 10, `cargo test --workspace` passes 1,135 tests (1,008 at `f2cc9fb`);
- `openspec validate add-core-agent --strict` after Task 1 and Task 10, and `cargo deny check` after Task 4 and Task 10: advisories, bans, licences and sources pass, with no warning that `f2cc9fb` does not already have;
- Step 2 of every task: the task's test files were put on the commit before it, and the failures listed under "Expected" are the ones they produced;
- the plan itself was replayed task by task onto a fresh clone at `f2cc9fb`, and every task's result matched its verified commit (`Cargo.lock` aside, which cargo writes).

The tasks were first built on `e37a8ef`, P3's last commit before its final fix wave. When P3 merged (`f2cc9fb`), Tasks 1 to 5 were moved onto it. Task 4 met the fix wave in two files, `crates/harness-cli/src/setup.rs` (which gained `trust`) and `crates/harness-cli/tests/cli_smoke.rs` (new tests); both are merged in the task below, and `main.rs`'s new `command_line` names the new subcommands. Everything was verified again on the new base.

`ask_e2e`'s `ctrl_c_during_a_never_closing_stdin_exits_130` fails whenever the tests run as a background job (`cargo test … &` from a script): SIGINT is ignored there, so the test's SIGINT, when it lands before harness has installed its handler, is dropped instead of killing the child, and the test's retry for that case never happens. It fails that way at `f2cc9fb` too. Some of the per-commit runs above were made that way, so `ask_e2e` was run again in the foreground at each commit that changes the CLI (Tasks 4 and 6 to 10): 20 of 20 pass every time.

What was not verified:

- **Real providers and servers.** No request reached OpenAI, Anthropic, ChatGPT or a real local server. The Responses and Messages fixtures in `crates/harness-providers/tests/fixtures/` are built from the providers' documented streaming examples, not recorded from live traffic. Overflow wording, quota error bodies (`usage_limit_reached`, `insufficient_quota`) and in-stream error types come from documentation and from Codex's parsing code. Local-server answers (llama.cpp `/props` with `?model=` on a server that is not in router mode, Ollama `/api/ps` `context_length` on older Ollama releases, LM Studio's `/api/v1/models`) come from their documentation.
- **ChatGPT's servers accepting harness.** Whether OpenAI's authorization server and ChatGPT's backend accept requests identified with Codex's own `originator` (`codex_cli_rs`) and scope list (decision 2) from a client they did not register, whether the device-code flow works for it, and whether ChatGPT's backend accepts harness's requests otherwise (its system prompt as `instructions`, no output limit) are unverified. The flows follow Codex's code exactly otherwise.
- **Keychains.** The user's keychain was never touched. `KeychainStore` was tested through `keyring-core`'s mock store; the real macOS Keychain and a Linux Secret Service were not exercised, nor whether macOS asks for a password when a rebuilt binary reads an entry.
- **Linux.** `harness-providers` pulls in `aws-lc-sys` through `reqwest`, so it cannot be linted for Linux from macOS. `credentials.rs` was type-checked for `x86_64-unknown-linux-gnu` in a throwaway crate with the same dependencies, including the Secret Service store; nothing else of P4 is Linux-specific. Its tests first run on Linux in the pull request's CI.
- **Terminals and browsers.** Reading a key without echo (`auth add` on a terminal) and opening a browser (`open`, `xdg-open`) have no automated test; the tests pipe keys in and use the device flow.
- **Built-in profile values** come from model cards and Codex's model list; no server was asked.

## Before you start: P3 must be merged

P3 merged as pull request #6: start from `master` at `f2cc9fb` or later. This plan's `Replace` blocks were checked against `f2cc9fb`. If `master` has moved on and a block in `ask.rs`, `main.rs`, `setup.rs`, `config.rs` or `README.md` no longer matches exactly, a nearby line changed: make the same change by hand next to it.

## Decisions this plan asks you to approve

The spec settles what P4 does; these settle how, where it is silent. Task 1 writes all of them into it except 1, 2, 18 and 19, which are about policy, a listing and dependencies.

1. **ChatGPT sign-in uses the Codex CLI's public OAuth client. Whether a third-party tool may do that is your call.** What I found in `openai/codex` (Apache-2.0) at commit `7e049b3eaa17cf01f726ee8ff70da4dcf7557f87` (2026-09-28):
   - client id `app_EMoamEEZ73f0CkXaXp7hrann` (`codex-rs/login/src/auth/manager.rs`, `CLIENT_ID`);
   - issuer `https://auth.openai.com`; authorize at `/oauth/authorize` with PKCE (S256), `id_token_add_organizations=true`, `codex_cli_simplified_flow=true` and `originator`; callback `http://127.0.0.1:1455/auth/callback`, and port 1457 when 1455 is taken, "kept in sync with the Codex CLI Hydra redirect URI allow-list"; the code exchanged form-encoded at `/oauth/token` (`codex-rs/login/src/server.rs`);
   - device codes from `/api/accounts/deviceauth/usercode` (the interval comes as a string), polled at `/api/accounts/deviceauth/token` (403 or 404 while waiting), entered at `/codex/device`, valid 15 minutes, then exchanged with the redirect `<issuer>/deviceauth/callback` (`codex-rs/login/src/device_code_auth.rs`);
   - refresh as JSON to `/oauth/token`, 5 minutes before the access token's `exp`; `refresh_token_expired`, `_reused` and `_invalidated` mean signing in again (`codex-rs/login/src/auth/manager.rs`);
   - requests to `https://chatgpt.com/backend-api/codex/responses` (`codex-rs/model-provider-info/src/lib.rs`) with the `ChatGPT-Account-ID` header (`codex-rs/model-provider/src/bearer_auth_provider.rs`), taken from the ID token's `https://api.openai.com/auth` claim `chatgpt_account_id` (`codex-rs/login/src/token_data.rs`), with `store: false` (`codex-rs/core/src/client.rs`).

   The Apache licence covers Codex's code, not the use of its OAuth client: the client id and its redirect URIs are registered with OpenAI for Codex, and I found no OpenAI statement that lets other applications use them. What supports doing it is design.md's finding that OpenAI publicly tolerates ChatGPT sign-in in third-party harnesses (pi, OpenCode, Amp). D3 already isolates it: it lives behind the default-on `chatgpt-login` Cargo feature (a `--no-default-features` build has none, and says so), and the login says it rests on OpenAI's current practice. *Alternatives:* ship the feature off by default; or leave ChatGPT sign-in out of M1 and rely on OpenAI API keys and M3's `codex app-server` delegation.
2. **harness identifies itself to OpenAI the way Codex does.** The maintainer chose the plan's alternative over the plan's original proposal: harness sends Codex's own `originator` (`codex_cli_rs`) when signing in and with requests to ChatGPT's backend, and asks for Codex's full scope list (`openid profile email offline_access api.connectors.read api.connectors.invoke`), including the two connector scopes harness does not use. The reasoning: harness already signs in with the Codex CLI's OAuth client (decision 1); presenting a distinct `originator=harness` and a smaller scope set is an unverified guess about what OpenAI's servers accept from a client they did not register, where matching Codex exactly is known to work. The wording throughout says harness signs in with the Codex CLI's OAuth client and identifies to OpenAI as it, not that OpenAI endorses harness.
3. **The Responses adapter is stateless.** Each request carries the whole conversation with `store: false`; reasoning items are not kept between requests (Codex sends them back encrypted with `include: ["reasoning.encrypted_content"]`), and reasoning summaries stream as reasoning deltas. So nothing provider-specific enters the session format, and switching models loses nothing. Requests to ChatGPT's backend carry no output limit, as Codex sends none. *Alternative:* keep encrypted reasoning items per protocol in the message model (a session format change), for better multi-step reasoning on OpenAI's reasoning models.
4. **The Anthropic adapter** authenticates with an API key only (`x-api-key`), sends `max_tokens` from the profile or 16,384 (the protocol requires one, and input plus `max_tokens` must fit the window), marks the system prompt and the last message as prompt-cache breakpoints, and requests no extended thinking in M1 (thinking blocks would have to go back signed within a tool loop). Overloaded, API and rate-limit errors inside a stream are retried like their HTTP forms.
5. **Where same-role messages are joined (P3's review F).** The agent already joins consecutive user messages for every provider (P3's `request_messages`), which covers a compaction summary followed by the next prompt. The Anthropic adapter also puts tool results and the user text after them into one user message, since tool results are user content in its protocol, and leaves out empty assistant messages. Harness never adds a note of its own directly after tool results: a cut-off tool call's notice goes in its tool result (decision 13). One case remains: a turn that ends on tool results (the step limit, Ctrl+C) followed by a new prompt reads as two user turns to a Mistral template that drops tool messages; P5 can close it when it adds steering. *Alternative:* each adapter joins messages its own way.
6. **The credential store.** `keyring-core` with the macOS Keychain or, on Linux, the Secret Service over D-Bus (pure Rust through `zbus`, no `libdbus`); service `harness`, account `<provider>/<profile>`. Without a usable keychain, `credentials.json` in the data directory, mode `0600`, rewritten through a temporary file and locked while in use, with a warning; a readable one is made private again. Reads look in the keychain, then the file; removing deletes from both. `HARNESS_CREDENTIAL_STORE=file` uses only the file, and every end-to-end test sets it. The active profiles live in `accounts.toml` next to it. A stored key is looked up only for providers with a key variable (`api_key_env`, or a built-in's), so a request to a local server never reads the keychain. *Alternative:* the file only on Linux (dropping most of the 45 packages P4 adds to `Cargo.lock`), or no keychain at all.
7. **`harness auth add` reads the key from standard input**: typed without echo on a terminal, or the first non-empty line of what is piped in (a password manager may print more lines), trimmed. Never from the command line, where it would reach the shell history.
8. **Claude credentials are refused, not just unused.** A key that starts with `sk-ant-oat` (a Claude subscription token) is refused for any `anthropic-messages` provider, whether it comes from the environment, the store or `auth add`, and is never sent, not even to list models. `harness login anthropic` and `harness login claude` explain that an API key is required. Nothing under `~/.claude` is read for credentials (P3 reads only `~/.claude/commands`). `Debug` of a resolved provider shows `[redacted]` for its key.
9. **Model profiles resolve each setting on its own**: the user's profiles, then the built-in ones, then defaults; within a layer the matching key with the most characters other than `*` and `?` wins, keys match without regard to case, and `*` crosses `/` (so `*/qwen3-coder*` matches `openrouter/qwen/qwen3-coder`). Built in: Qwen3-Coder (262,144 tokens, temperature 0.7), Qwen3 (32,768), Qwen2.5-Coder (32,768), Devstral (131,072), gpt-oss (131,072, temperature 1.0), GLM-4.5, DeepSeek-Coder-V2 and Llama 3.1 (131,072), Claude (200,000), ChatGPT and GPT-5 (272,000), GPT-4.1 (1,047,576), GPT-4o (128,000), o3 and o4-mini (200,000), from the model cards and Codex's model list. A project's `[profiles]` apply only in a trusted workspace, since they choose output limits, reasoning effort and context budgets (paid requests) and whether text is run as tool calls, as P3-R4 ruled for low compaction thresholds. *Alternative:* project profiles apply without trust.
10. **"Local" means a local server**: `ollama`, `lmstudio` or `llamacpp` wherever they run, or any provider whose base URL is on the loopback interface; a profile's `local` overrides it. Text tool calls default to on for local models.
11. **Effective context.** Only `ollama`, `lmstudio` and `llamacpp` are asked (by name, wherever they run, when they speak Chat Completions), each question limited to one second. Ollama lists only loaded models, so a model it has not loaded is loaded first with an empty `/api/generate` (up to 120 seconds, stopped by Ctrl+C), which the first request would do anyway. An LM Studio model that is not loaded counts as unknown. Harness uses the smaller of the server's value and the profile's; with neither, 8,192 tokens and one warning. A window below the profile's `min_context` (32,768 by default) is warned about with that server's fix. *Alternative:* never load a model to ask (misses the common first run, when nothing is loaded yet).
12. **Text tool calls must name a tool the agent has**, besides being the whole message (`<tool_call>` blocks with only whitespace around them, or one JSON object with `name` and `arguments`, or `parameters` as Llama 3.1 writes). A recovered call is saved as a call, without the wrapper text, and validated and permission-checked like a native one. *Alternative:* recover any name and let validation report an unknown tool (more recoveries, and more false ones).
13. **A reply cut off at the output limit** runs none of its tool calls: each gets an error result saying so and asking for smaller steps. A reply without calls is kept and followed by a note (`[harness] Your last reply was cut off…`) asking the model to continue. The turn goes on within its step limit, and a warning says what happened. A cut-off reply is not searched for text tool calls.
14. **Continuing a local conversation on a hosted model says nothing (the product decision P3's review left open).** The maintainer chose the plan's first alternative over the plan's original proposal, which would have warned that the conversation, tool output included, now goes to that provider when every answer so far came from local models and the model now answering is not local: say nothing, the behaviour before P4. *Alternative not chosen:* refuse unless a flag confirms it (P5 could ask interactively). Decision 10's "local means a local server" logic is unaffected: text tool calls and profiles still use it.
15. **Secrets are redacted where harness writes, not in what the model sees.** Secrets are the keys and tokens providers are given (added as they are resolved or refreshed) and the values of environment variables whose names end in `KEY`, `TOKEN`, `SECRET` or `PASSWORD`, eight characters or longer, in their plain and JSON-escaped forms. They are replaced by `[redacted]` in session files, tool-output files, the debug log, NDJSON and everything harness prints. Redacting what the model sees would change a file the model reads and writes back. The name rule has false positives (`LESSKEY` holds a path), which only hide text. *Alternative:* also redact tool output before the model sees it, which keeps keys from reaching the provider, at that cost.
16. **`--debug` writes the event stream**, redacted, to `$XDG_STATE_HOME/harness/logs/<run>.log` (mode `0600`) and prints the path; it logs no HTTP traffic. The spec's "Debug logging" scenario needed a log to check.
17. **An exhausted quota is not retried.** A 429 that reports `usage_limit_reached`, `usage_not_included` or `insufficient_quota` fails the turn at once with the reset time when the provider gives it (`resets_at`, or `resets_in_seconds`), and suggests `--model` (or `/model` in the terminal UI). Other 429s are still retried.
18. **`harness models` does not list ChatGPT's models**: Codex gets them from an endpoint that expects a Codex client version. The README says to use a model the plan includes as `chatgpt/<model>`.
19. **New third-party crates** (all checked with `cargo deny check`, which passes with no warning `f2cc9fb` does not already have):
    - `keyring-core` 1.0.0, MIT OR Apache-2.0: the credential store interface and its mock store for tests;
    - `apple-native-keyring-store` 1.0.2 (feature `keychain`), MIT OR Apache-2.0, macOS only: the Keychain (through `security-framework`, already in the tree);
    - `zbus-secret-service-keyring-store` 1.0.1 (feature `rt-async-io-crypto-rust`), MIT OR Apache-2.0, Linux only: the Secret Service in pure Rust. It brings most of the 45 packages `Cargo.lock` gains (the `zbus` stack);
    - `base64` 0.23.1, MIT OR Apache-2.0, optional with `chatgpt-login`: PKCE and JWT claims. It is already in the tree through `hyper-util`.

    Workspace crates reach further: `globset` now in `harness-config` and `harness-providers`, `sha2` in `harness-providers`, `nix`'s `term` feature in `harness-cli`. Randomness for PKCE comes from `/dev/urandom` and form bodies are built with `reqwest`'s `Url`, so no `rand` crate and no new `reqwest` features.

## Global Constraints

- Rust `1.98.0`, edition 2024, licence `MIT OR Apache-2.0`, macOS and Linux; every crate keeps `publish = false`.
- Commits: `git commit -F -` with a conventional-commit subject, a short body, and the trailer lines the controller gives you (written `<trailer lines from the controller>` below).
- Third-party crates: only those in decision 19; `cargo deny check` stays green.
- No test calls a real provider or authorization server, reads a real credential, or touches the user's keychain: providers, OAuth and local servers are `wiremock` servers; unit tests use `keyring-core`'s mock store or a file store in a temporary directory; every end-to-end invocation of `harness` sets `HARNESS_CREDENTIAL_STORE=file` and a temporary `HARNESS_HOME`.
- Credentials never live in the configuration directory. The files harness creates for them and for `--debug` (`credentials.json`, `accounts.toml`, the debug log) are mode `0600`, and the log directory `0700`.
- Claude subscription credentials are never read, stored or sent.
- No API key, OAuth token or secret-named environment value reaches a session file, a tool-output file, the debug log, NDJSON or anything harness prints; types that hold one implement `Debug` without it.
- Text from providers, servers, files or the model is printed through `terminal_safe` (`terminal_safe_text` for multi-line text).
- Headless exit codes do not change: `0`, `1`, `2`, `3`, `130`. A usage error in `auth`, `login` or `logout` exits 2.
- The system prompt and tool definitions stay byte-identical across a session's requests in one process (P3).
- The `chatgpt-login` feature is on by default, and `--no-default-features` builds of `harness-providers` and `harness-cli` lint clean.
- Network connections go only to configured or built-in providers, their authorization endpoints, and local servers (no telemetry).

## Review Focus

- **A local model's reply quotes the tool-call format** (in prose, a code fence, or before its own explanation): it must stay text and run nothing. Test in Task 9 (`text_around_a_call_or_a_quoted_call_is_not_recovered`).
- **Two harness processes use the same ChatGPT account** and both find the access token expired: refresh tokens are single-use, so the second must take the tokens the first stored instead of asking again and being refused. Test in Task 6 (`tokens_another_process_refreshed_are_used_without_asking_again`).
- **Another tab, or a forged link, reaches the sign-in callback** with a different `state`: it must be refused and the sign-in must keep waiting for the real redirect. Test in Task 5 (`a_callback_with_the_wrong_state_is_refused`).
- **A credentials file that became readable by others** (restored from a backup, copied with `cp`): it must be made private again before it is used. Test in Task 4 (`a_readable_credentials_file_is_made_private_again`).
- **A local server that is not running or hangs**: starting a run must wait no longer than the probe's limit. Test in Task 8 (`a_server_that_does_not_answer_reports_nothing_in_time`).

---

## File Map

```
openspec/changes/add-core-agent/{design.md,tasks.md,specs/*}   refinements (Task 1)
crates/harness-core/src/
  message.rs                     RequestOptions; ChatRequest::options (Task 2)
  provider.rs                    exhausted quotas: is_quota_exhausted, resets_at (Task 6)
  textcalls.rs                   NEW  tool calls written as text (Task 9)
  redact.rs                      NEW  Redactor (Task 10)
  agent.rs                       request options, cut-off replies, text calls, redaction, quota message
  session.rs, output.rs          redacted session lines and tool-output files (Task 10)
crates/harness-config/src/config.rs   Protocol variants (Tasks 2, 3); [profiles] (Task 7)
crates/harness-providers/src/
  sse.rs                         NEW  the adapters' shared SSE reader (Task 2)
  openai_responses.rs            NEW  the Responses adapter (Task 2; ChatGPT accounts, Task 6)
  anthropic_messages.rs          NEW  the Messages adapter (Task 3)
  credentials.rs                 NEW  keychain, file fallback, account profiles (Task 4)
  chatgpt/{mod,oauth,auth}.rs    NEW  sign-in flows (Task 5); the signed-in account (Task 6)
  profiles.rs                    NEW  model profiles (Task 7)
  window.rs                      NEW  effective context (Task 8)
  registry.rs, discovery.rs      built-ins, keys and secrets, chatgpt/*, Anthropic listing
  openai_chat.rs                 on the shared reader (Task 2); request options (Task 7)
crates/harness-providers/tests/fixtures/{openai-responses,anthropic-messages}/*.sse   NEW
crates/harness-cli/src/
  auth.rs, login.rs              NEW  auth add/use, logout (Task 4); login (Task 6)
  setup.rs, main.rs              credentials, keys and redactor; the new subcommands and --debug
  ask.rs, context.rs, sessions.rs   profiles, window, redaction, debug log
crates/harness-cli/tests/{auth,login,profiles,local,redaction}_e2e.rs   NEW
README.md                        what P4 adds, and its limitations (Task 10)
```

---

### Task 1: Write the refinements into the spec

**Files:**
- Modify: `openspec/changes/add-core-agent/design.md`, `openspec/changes/add-core-agent/tasks.md`, and in `openspec/changes/add-core-agent/specs/`: `model-providers/spec.md`, `provider-auth/spec.md`, `configuration/spec.md`, `cli-interface/spec.md`

**Interfaces:**
- Consumes: nothing.
- Produces: the binding text for decisions 3 to 17, and the P4 plan's path in `tasks.md`.

- [ ] **Step 1: Update design.md**

In `openspec/changes/add-core-agent/design.md` (D2, D3, D9, D10 and D14):

Replace (1 of 4):

```markdown
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

```

with:

```markdown
| `openai-responses` | `/v1/responses` (SSE) | OpenAI API keys; ChatGPT sign-in |
| `anthropic-messages` | `/v1/messages` (SSE) | Anthropic API keys; Anthropic-compatible local servers |

Model ids are `<provider>/<model>` (e.g. `ollama/qwen3-coder:30b`, `chatgpt/<model>`). On first interactive use with no configured model, the model picker opens and saves the choice as the global default; `harness ask` never picks a model implicitly. When switching models, provider-specific content the target cannot accept (e.g. signed reasoning blocks) is dropped. Continuing a conversation on a hosted model after local ones is silent.

The built-in hosted providers are `openai` (`openai-responses` at `https://api.openai.com/v1`, `OPENAI_API_KEY`), `anthropic` (`anthropic-messages` at `https://api.anthropic.com/v1`, `ANTHROPIC_API_KEY`), `openrouter` (`openai-chat`) and `chatgpt` (`openai-responses` at `https://chatgpt.com/backend-api/codex`, signed in). The `openai-responses` adapter is stateless: every request carries the whole conversation with `store: false`, and reasoning items are not kept between requests, so nothing provider-specific has to survive a model switch; reasoning summaries stream as reasoning deltas. Requests to ChatGPT's backend carry no output limit, which it does not take (Codex sends none). The `anthropic-messages` adapter authenticates only with an API key, sends `max_tokens` from the profile or 16,384, marks the system prompt and the last message as prompt-cache breakpoints, and does not request extended thinking in M1. Consecutive user messages are joined into one before any adapter sees them, since some chat templates reject two in a row; the Anthropic adapter also puts tool results and the user text after them into one user message, because tool results are user content in its protocol, and leaves out empty assistant messages. Harness never adds a note of its own directly after tool results: a truncation notice travels inside the tool results. An error inside a stream that reports the provider overloaded is retried like HTTP 529. A 429 that reports an exhausted quota or plan limit (`usage_limit_reached`, `insufficient_quota`, `usage_not_included`) is not retried; the error names the reset time when the provider gives one.

**Model profiles** are TOML tables keyed by model-id glob (`[profiles."ollama/qwen3-coder*"]`, matched without regard to case) with fields `context_window`, `min_context`, `max_output_tokens`, `temperature`, `reasoning_effort`, `text_tool_calls` (on/off), and `local` (bool). Each field is resolved on its own: user config → built-in profiles shipped in the binary for common open-weight coding families and the hosted providers' model families → protocol defaults. Within a layer, the matching key with the most characters other than `*` and `?` wins. The defaults: `local` is on for `ollama`, `lmstudio`, `llamacpp` and any provider whose base URL is on the loopback interface; `text_tool_calls` follows `local`; `min_context` is 32,768; the context window is unknown. `Provider` is a trait so M2's router can be a provider that delegates to other providers; delegated agents (M3) will use a separate `AgentBackend` trait. M1 defines neither.

### D3. Credentials and account profiles

- Resolution per provider and account profile: configured env var (e.g. `OPENAI_API_KEY`) → stored credential for the active profile. `harness auth add <provider> [--profile <name>]` reads the key from standard input, without echo on a terminal, never from the command line, where it would reach the shell history. A provider without a key variable takes no key, so no request to it reads the keychain.
- Storage: the OS keychain through `keyring-core` (the macOS Keychain; the Secret Service over D-Bus on Linux), service `harness`, account `<provider>/<profile>`. When no keychain service can be used, `$XDG_DATA_HOME/harness/credentials.json` with mode `0600` and a warning; reads look in the keychain, then in that file. `HARNESS_CREDENTIAL_STORE=file` uses only the file. Credentials never live in the config directory, which users often sync to dotfile repositories.
- Profiles: `harness login chatgpt --profile work` and `harness auth add <provider> --profile work` store under a profile; `harness auth use <provider> <profile>` selects the active one, recorded in `$XDG_DATA_HOME/harness/accounts.toml`; the unnamed profile is `default`. `harness logout <provider> [--profile <name>]` removes the stored credential.
- ChatGPT sign-in: OAuth 2.0 authorization code with PKCE against `https://auth.openai.com`, using the public client id of OpenAI's open-source Codex CLI and its registered callback `http://127.0.0.1:1455/auth/callback` (port 1457 when 1455 is taken); the device-code flow (`/api/accounts/deviceauth/*`, verified at `/codex/device`) with `--device`, over SSH, or when no browser can be opened. The access, refresh and ID tokens and the ChatGPT account id (from the ID token) are stored as one credential. Requests carry the `ChatGPT-Account-ID` header. The access token is refreshed when it expires within 5 minutes, and after a 401, once, before the request is retried once; before refreshing, the stored tokens are read again, since another harness process may have refreshed them already. The login flow says that ChatGPT subscription use in third-party tools relies on OpenAI's current practice. Isolated behind the default-on Cargo feature `chatgpt-login` so it can be disabled quickly if OpenAI's policy changes.
- Claude subscription credentials are never read, stored, or used: nothing under `~/.claude` or in Claude Code's keychain item is read for credentials, a Claude subscription token (`sk-ant-oat…`) is refused wherever it comes from, and `harness login anthropic` (or `claude`) explains that an API key is required.
- Secrets are redacted where harness writes, not in what the model sees: API keys and tokens harness uses, and the values of environment variables whose names end in `KEY`, `TOKEN`, `SECRET` or `PASSWORD` (values of eight characters or more), are replaced by `[redacted]` in session files, tool-output files, the debug log, NDJSON, and everything printed. The model still sees tool output as it is, so a file it reads and writes back keeps its real contents.

### D4. Tools

```

Replace (2 of 4):

```markdown

Paths follow the XDG base-directory spec on macOS and Linux: config `$XDG_CONFIG_HOME/harness` (default `~/.config/harness`), data `$XDG_DATA_HOME/harness` (default `~/.local/share/harness`: sessions, checkpoints, trust list, credential fallback), state `$XDG_STATE_HOME/harness` (default `~/.local/state/harness`: logs, tool output). `HARNESS_HOME`, when set, overrides all three with subdirectories of one path.

TOML layering: global `config.toml` ← project `.harness/config.toml` ← CLI flags. Project settings that could widen the harness's reach (a `mode` that grants more than the effective global mode, a `max_steps` above the effective global limit, `model`, `[permissions].allow`, `read_dirs`, provider definitions or `base_url` overrides, and any `[sandbox]` setting except `allow_localhost = false`) are applied only after the user trusts the workspace. The effective global mode is the global config's `mode`, else the default for the workspace (`auto` in a git work tree, `ask` elsewhere), and modes rank `plan` = `read-only` < `ask` < `auto` < `full-access`; the effective global limit is the global `max_steps`, else 50. Trust is stored in the data directory as a fingerprint (SHA-256) of those settings; if they change, the workspace is untrusted again. A workspace with none can be trusted too (the fingerprint of the empty set), so that its project command files may choose their model. `harness trust` shows the settings and records trust; the interactive first-use prompt arrives with the terminal UI. Headless runs ignore untrusted widening settings with a warning. Narrowing settings (`deny`, `confirm`, a mode or `max_steps` no wider than the effective global one, and `allow_localhost = false`) always apply. `[compaction]` settings change when harness summarizes, not what the agent may do, so a project sets them without trust, except a `threshold_percent` below 50: summarizing every few turns costs paid requests and verbatim context, so such a threshold is shown and fingerprinted with the widening settings, and until trusted the global value or the default applies, with a warning.

Example global config:

```

with:

```markdown

Paths follow the XDG base-directory spec on macOS and Linux: config `$XDG_CONFIG_HOME/harness` (default `~/.config/harness`), data `$XDG_DATA_HOME/harness` (default `~/.local/share/harness`: sessions, checkpoints, trust list, credential fallback), state `$XDG_STATE_HOME/harness` (default `~/.local/state/harness`: logs, tool output). `HARNESS_HOME`, when set, overrides all three with subdirectories of one path.

TOML layering: global `config.toml` ← project `.harness/config.toml` ← CLI flags. Project settings that could widen the harness's reach (a `mode` that grants more than the effective global mode, a `max_steps` above the effective global limit, `model`, `[permissions].allow`, `read_dirs`, provider definitions or `base_url` overrides, `[profiles]` entries, and any `[sandbox]` setting except `allow_localhost = false`) are applied only after the user trusts the workspace. A project's model profiles need trust because they choose output limits, reasoning effort and context budgets, which cost paid requests, and whether text is run as tool calls. The effective global mode is the global config's `mode`, else the default for the workspace (`auto` in a git work tree, `ask` elsewhere), and modes rank `plan` = `read-only` < `ask` < `auto` < `full-access`; the effective global limit is the global `max_steps`, else 50. Trust is stored in the data directory as a fingerprint (SHA-256) of those settings; if they change, the workspace is untrusted again. A workspace with none can be trusted too (the fingerprint of the empty set), so that its project command files may choose their model. `harness trust` shows the settings and records trust; the interactive first-use prompt arrives with the terminal UI. Headless runs ignore untrusted widening settings with a warning. Narrowing settings (`deny`, `confirm`, a mode or `max_steps` no wider than the effective global one, and `allow_localhost = false`) always apply. `[compaction]` settings change when harness summarizes, not what the agent may do, so a project sets them without trust, except a `threshold_percent` below 50: summarizing every few turns costs paid requests and verbatim context, so such a threshold is shown and fingerprinted with the widening settings, and until trusted the global value or the default applies, with a warning.

Example global config:

```

Replace (3 of 4):

```markdown

### D10. Headless mode and exit codes

`harness ask "<prompt>"` appends piped stdin to the prompt, prints the final assistant text to stdout (progress to stderr), or the full event stream as NDJSON with `--json`. Actions that need approval are denied and the model is told why. Exit codes: `0` success, `1` runtime error, `2` invalid usage or no usable model, `3` finished with at least one action blocked for lack of approval, `130` interrupted.

### D11. Engineering baseline

```

with:

```markdown

### D10. Headless mode and exit codes

`harness ask "<prompt>"` appends piped stdin to the prompt, prints the final assistant text to stdout (progress to stderr), or the full event stream as NDJSON with `--json`. `--debug` also writes the event stream, secrets redacted, to `$XDG_STATE_HOME/harness/logs/<run>.log` and prints that path. Actions that need approval are denied and the model is told why. Exit codes: `0` success, `1` runtime error, `2` invalid usage or no usable model, `3` finished with at least one action blocked for lack of approval, `130` interrupted.

### D11. Engineering baseline

```

Replace (4 of 4):

```markdown

### D14. Local-model robustness

- **Effective context:** harness queries the context size the server is actually running with (llama.cpp `/props`, Ollama running-model info, LM Studio model info) and uses the smaller of that and the profile. If it is below the profile's `min_context` (default 32k tokens for agentic use), the user is warned with the fix (e.g. `OLLAMA_CONTEXT_LENGTH`).
- **Text tool calls:** when `text_tool_calls` is on (default for local providers), an assistant message with no native tool calls whose content is a recognised tool-call wrapper (`<tool_call>…</tool_call>` blocks, or a message consisting solely of a JSON object with `name` and `arguments`) is parsed into tool calls. Parsed calls go through the same schema validation.
- **Truncation:** when the provider reports output stopped at the length limit, a partial tool call is never executed; the model is told its output was cut off and asked to continue in smaller steps.
- **Bounded repair:** invalid tool calls are fed back as errors; the per-turn invalid-call count is exposed for M2's escalation rule.

### D15. Cache-stable prompt prefix
```

with:

```markdown

### D14. Local-model robustness

- **Effective context:** harness queries the context size the server is actually running with (llama.cpp `GET /props`, `default_generation_settings.n_ctx`; Ollama `GET /api/ps`, the model's `context_length`, after asking Ollama to load the model with an empty `/api/generate` when it is not loaded yet; LM Studio `GET /api/v1/models`, a loaded instance's `config.context_length`) and uses the smaller of that and the profile. Only providers named `ollama`, `lmstudio` and `llamacpp` are asked, wherever they run, as long as they speak Chat Completions; a server that does not answer within its time limit counts as not reporting. When neither the server nor a profile gives a window, harness uses 8,192 tokens and warns once per session. If the window is below the profile's `min_context` (default 32k tokens for agentic use), the user is warned with the fix (e.g. `OLLAMA_CONTEXT_LENGTH`). The window sets the compaction budgets and the instruction-size warning.
- **Text tool calls:** when `text_tool_calls` is on (default for local providers), an assistant message with no native tool calls whose content is a recognised tool-call wrapper (one or more `<tool_call>…</tool_call>` blocks with nothing but whitespace around them, or a message consisting solely of a JSON object with `name` and `arguments`, or `parameters`, which Llama 3.1's format uses) is parsed into tool calls when every call names a tool the agent has. Parsed calls go through the same schema validation and permission checks; the assistant message is saved with the calls and without the wrapper text.
- **Truncation:** when the provider reports output stopped at the length limit, a partial tool call is never executed: each tool call of that output gets an error result saying the output was cut off, asking the model to continue in smaller steps. Output without tool calls is kept and followed by a note asking the model to continue where it stopped, in smaller steps. Either way the turn goes on, within its step limit, and a warning says so.
- **Bounded repair:** invalid tool calls are fed back as errors; the per-turn invalid-call count is exposed for M2's escalation rule.

### D15. Cache-stable prompt prefix
```

- [ ] **Step 2: Update the specs**

In `openspec/changes/add-core-agent/specs/model-providers/spec.md`:

Replace (1 of 3):

```markdown
- **THEN** the next request to the new model includes the prior user messages, assistant replies, and tool results

### Requirement: Model profiles tune behaviour per model
The system SHALL resolve a model profile for the active model from user configuration, then built-in profiles, then protocol defaults, matching profile keys as globs against model ids. A profile MUST be able to set the context window, minimum context, maximum output tokens, temperature, reasoning effort, text tool-call parsing, and whether the model is local. The system MUST ship built-in profiles for common open-weight coding model families.

#### Scenario: User profile overrides built-in
- **WHEN** a built-in profile sets temperature 0.7 for `ollama/qwen3-coder*` and the user's config sets temperature 0.2 for the same glob
- **THEN** requests to `ollama/qwen3-coder:30b` use temperature 0.2

### Requirement: Effective context is detected and checked
The system SHALL determine a model's effective context window as the smaller of the size the serving local server reports it is actually running with and the profile's context window, fall back to 8192 tokens with a warning when neither is known, and warn the user with a remediation hint when the effective window is below the profile's minimum context (default 32,768 tokens).

#### Scenario: Ollama running with a small context
- **WHEN** Ollama reports the loaded model runs with a 4,096-token context and the profile's minimum is 32,768
```

with:

```markdown
- **THEN** the next request to the new model includes the prior user messages, assistant replies, and tool results

### Requirement: Model profiles tune behaviour per model
The system SHALL resolve a model profile for the active model from user configuration, then built-in profiles, then protocol defaults, matching profile keys as globs against model ids without regard to case. Each setting MUST be resolved on its own, and within one layer the matching key with the most characters other than `*` and `?` MUST win. A profile MUST be able to set the context window, minimum context, maximum output tokens, temperature, reasoning effort, text tool-call parsing, and whether the model is local. The system MUST ship built-in profiles for common open-weight coding model families. Profiles in a project's configuration MUST apply only in a trusted workspace.

#### Scenario: User profile overrides built-in
- **WHEN** a built-in profile sets temperature 0.7 for `ollama/qwen3-coder*` and the user's config sets temperature 0.2 for the same glob
- **THEN** requests to `ollama/qwen3-coder:30b` use temperature 0.2

#### Scenario: The most specific key wins
- **WHEN** the user's config sets `context_window = 65536` for `ollama/*` and `context_window = 16384` for `ollama/qwen3-coder*`
- **THEN** `ollama/qwen3-coder:30b` uses a 16,384-token window and `ollama/llama3.1` a 65,536-token window

### Requirement: Effective context is detected and checked
The system SHALL determine a model's effective context window as the smaller of the size the serving local server reports it is actually running with and the profile's context window, fall back to 8192 tokens with a warning when neither is known, and warn the user with a remediation hint when the effective window is below the profile's minimum context (default 32,768 tokens). It MUST ask llama.cpp's `/props`, Ollama's running models (loading the model first when it is not loaded) and LM Studio's loaded model instances, and a server that does not answer in time MUST count as not reporting.

#### Scenario: Ollama running with a small context
- **WHEN** Ollama reports the loaded model runs with a 4,096-token context and the profile's minimum is 32,768
```

Replace (2 of 3):

```markdown
- **THEN** no file is read, the model receives an error result, and the turn's invalid-call count increases by one

### Requirement: Tool calls written as text are recovered
When text tool-call parsing is enabled for the active model (the default for local providers), the system SHALL treat an assistant message that contains no native tool calls and consists of `<tool_call>` blocks, or solely of a JSON object with `name` and `arguments` fields, as tool calls. Recovered calls MUST go through the same validation and permission checks as native calls. Text that merely contains such structures alongside other prose MUST NOT be treated as a tool call.

#### Scenario: Local model emits a tagged tool call as text
- **WHEN** a local model replies only with `<tool_call>{"name":"read","arguments":{"path":"src/lib.rs"}}</tool_call>`
```

with:

```markdown
- **THEN** no file is read, the model receives an error result, and the turn's invalid-call count increases by one

### Requirement: Tool calls written as text are recovered
When text tool-call parsing is enabled for the active model (the default for local providers), the system SHALL treat an assistant message that contains no native tool calls and consists of `<tool_call>` blocks, or solely of a JSON object with `name` and `arguments` (or `parameters`) fields, naming available tools, as tool calls. Recovered calls MUST go through the same validation and permission checks as native calls. Text that merely contains such structures alongside other prose MUST NOT be treated as a tool call.

#### Scenario: Local model emits a tagged tool call as text
- **WHEN** a local model replies only with `<tool_call>{"name":"read","arguments":{"path":"src/lib.rs"}}</tool_call>`
```

Replace (3 of 3):

```markdown
- **THEN** no tool call is executed

### Requirement: Truncated output is detected
When the provider reports that output stopped because it reached the output-token limit, the system SHALL NOT execute any partial tool call from that output, and MUST tell the model its output was cut off and ask it to continue in smaller steps.

#### Scenario: Write call cut off
- **WHEN** a `write` call's arguments are cut off by the output limit
- **THEN** no file is written and the model receives a message that its output was truncated

### Requirement: The user chooses a default model on first use
When no model is configured or given on the command line, interactive mode SHALL show the model picker listing discovered and credentialed models and save the selection as the global default. If no models are available, interactive mode MUST guide the user to sign in or configure a provider. `harness ask` MUST NOT pick a model implicitly: it MUST exit with code 2 and a message listing any available models and how to set a default.

```

with:

```markdown
- **THEN** no tool call is executed

### Requirement: Truncated output is detected
When the provider reports that output stopped because it reached the output-token limit, the system SHALL NOT execute any partial tool call from that output, and MUST tell the model its output was cut off and ask it to continue in smaller steps. Each tool call of that output MUST receive an error result saying so; output without tool calls MUST be kept and followed by a note asking the model to continue. The turn MUST go on within its step limit.

#### Scenario: Write call cut off
- **WHEN** a `write` call's arguments are cut off by the output limit
- **THEN** no file is written and the model receives a message that its output was truncated

#### Scenario: Answer cut off
- **WHEN** a reply without tool calls stops at the output limit
- **THEN** the reply is kept, the model is asked to continue where it stopped, and its next reply finishes the turn

### Requirement: The user chooses a default model on first use
When no model is configured or given on the command line, interactive mode SHALL show the model picker listing discovered and credentialed models and save the selection as the global default. If no models are available, interactive mode MUST guide the user to sign in or configure a provider. `harness ask` MUST NOT pick a model implicitly: it MUST exit with code 2 and a message listing any available models and how to set a default.

```

In `openspec/changes/add-core-agent/specs/provider-auth/spec.md`:

Replace (1 of 5):

```markdown
## ADDED Requirements

### Requirement: API keys come from the environment or the credential store
The system SHALL resolve a provider's API key from its configured environment variable first and from the credential store for the active account profile second. `harness auth add <provider>` MUST store a key in the OS keychain. When no keychain service is available, the key MUST be stored in `credentials.json` in the harness data directory with file mode 0600, never in the configuration directory, and the user MUST be warned.

#### Scenario: Environment variable wins
- **WHEN** both `OPENAI_API_KEY` and a stored key for `openai` exist
```

with:

```markdown
## ADDED Requirements

### Requirement: API keys come from the environment or the credential store
The system SHALL resolve a provider's API key from its configured environment variable first and from the credential store for the active account profile second. `harness auth add <provider>` MUST read the key from standard input, without echoing it on a terminal, and MUST store it in the OS keychain. When no keychain service is available, the key MUST be stored in `credentials.json` in the harness data directory with file mode 0600, never in the configuration directory, and the user MUST be warned.

#### Scenario: Environment variable wins
- **WHEN** both `OPENAI_API_KEY` and a stored key for `openai` exist
```

Replace (2 of 5):

```markdown
- **WHEN** the user runs `harness auth add openrouter` and no keychain service is available
- **THEN** the key is written to `credentials.json` in the data directory with mode 0600 and a warning is shown

### Requirement: Multiple account profiles per provider
The system SHALL store credentials per provider and named account profile, with `default` as the unnamed profile. `harness login <provider> --profile <name>` and `harness auth add <provider> --profile <name>` MUST store credentials under that profile, and `harness auth use <provider> <name>` MUST make that profile the active one for the provider.

```

with:

```markdown
- **WHEN** the user runs `harness auth add openrouter` and no keychain service is available
- **THEN** the key is written to `credentials.json` in the data directory with mode 0600 and a warning is shown

#### Scenario: Key piped from a password manager
- **WHEN** the user runs `pass show openai | harness auth add openai`
- **THEN** the key is stored without its trailing newline, and it never appears in the command line or the shell history

### Requirement: Multiple account profiles per provider
The system SHALL store credentials per provider and named account profile, with `default` as the unnamed profile. `harness login <provider> --profile <name>` and `harness auth add <provider> --profile <name>` MUST store credentials under that profile, and `harness auth use <provider> <name>` MUST make that profile the active one for the provider.

```

Replace (3 of 5):

```markdown
- **THEN** subsequent `chatgpt/*` requests use the `work` account's credentials

### Requirement: ChatGPT sign-in
The system SHALL provide `harness login chatgpt`, which signs the user in through a browser-based OAuth flow with PKCE and a localhost callback, and SHALL offer a device-code flow when `--device` is given or a browser cannot be opened. Tokens MUST be stored in the credential store and refreshed automatically before expiry or after a single 401 response. The login flow MUST tell the user that ChatGPT subscription use in third-party tools relies on OpenAI's current practice rather than a contractual guarantee.

#### Scenario: Sign-in over SSH
- **WHEN** the user runs `harness login chatgpt` in an SSH session without a browser
```

with:

```markdown
- **THEN** subsequent `chatgpt/*` requests use the `work` account's credentials

### Requirement: ChatGPT sign-in
The system SHALL provide `harness login chatgpt`, which signs the user in through a browser-based OAuth flow with PKCE and a localhost callback, and SHALL offer a device-code flow when `--device` is given or a browser cannot be opened. Tokens MUST be stored in the credential store and refreshed automatically before expiry or after a single 401 response. Before refreshing, the system MUST read the stored tokens again and use them when another process has already refreshed them. The login flow MUST tell the user that ChatGPT subscription use in third-party tools relies on OpenAI's current practice rather than a contractual guarantee.

#### Scenario: Sign-in over SSH
- **WHEN** the user runs `harness login chatgpt` in an SSH session without a browser
```

Replace (4 of 5):

```markdown
- **WHEN** a request with the stored ChatGPT token returns 401 and the refresh token is valid
- **THEN** the system refreshes the token, retries the request once, and the turn continues

### Requirement: Credentials can be removed
The system SHALL provide `harness logout <provider> [--profile <name>]`, which removes that provider's stored credentials for the given profile (default: the active profile) from the credential store.

```

with:

```markdown
- **WHEN** a request with the stored ChatGPT token returns 401 and the refresh token is valid
- **THEN** the system refreshes the token, retries the request once, and the turn continues

#### Scenario: Two sessions refresh at once
- **WHEN** another harness process refreshed the stored ChatGPT tokens after this process read them, and this process's request returns 401
- **THEN** this process uses the stored tokens without asking the authorization server again

### Requirement: Credentials can be removed
The system SHALL provide `harness logout <provider> [--profile <name>]`, which removes that provider's stored credentials for the given profile (default: the active profile) from the credential store.

```

Replace (5 of 5):

```markdown
- **THEN** subsequent `chatgpt/*` requests report that the user is not signed in

### Requirement: Claude subscription credentials are never used
The system MUST NOT read, store, request, or use Claude.ai subscription credentials or session tokens, including those belonging to an installed Claude Code. The `anthropic` provider MUST authenticate only with an Anthropic API key.

#### Scenario: Claude Code signed in, no API key
- **WHEN** Claude Code is installed and signed in, and no Anthropic API key is configured
- **THEN** selecting an `anthropic/*` model reports that an API key is required
- **AND** no file under `~/.claude` is read for credentials

### Requirement: Secrets are redacted everywhere
The system MUST NOT write API keys, OAuth access tokens, or refresh tokens to logs, session files, tool-output files, NDJSON output, or error messages.

#### Scenario: Debug logging
- **WHEN** a turn runs with `--debug` using an API key provider
- **THEN** neither the log file nor the session file contains the key's value
```

with:

```markdown
- **THEN** subsequent `chatgpt/*` requests report that the user is not signed in

### Requirement: Claude subscription credentials are never used
The system MUST NOT read, store, request, or use Claude.ai subscription credentials or session tokens, including those belonging to an installed Claude Code. The `anthropic` provider MUST authenticate only with an Anthropic API key. A Claude subscription token (`sk-ant-oat…`) MUST be refused wherever it is given, and `harness login anthropic` MUST explain that an API key is required.

#### Scenario: Claude Code signed in, no API key
- **WHEN** Claude Code is installed and signed in, and no Anthropic API key is configured
- **THEN** selecting an `anthropic/*` model reports that an API key is required
- **AND** no file under `~/.claude` is read for credentials

#### Scenario: A subscription token in the environment
- **WHEN** `ANTHROPIC_API_KEY` holds a Claude subscription token and the user selects an `anthropic/*` model
- **THEN** harness refuses it with an explanation and sends no request

### Requirement: Secrets are redacted everywhere
The system MUST NOT write API keys, OAuth access tokens, or refresh tokens to logs, session files, tool-output files, NDJSON output, error messages, or anything else it prints. The values of environment variables whose names end in `KEY`, `TOKEN`, `SECRET` or `PASSWORD` MUST be treated as secrets too.

#### Scenario: Debug logging
- **WHEN** a turn runs with `--debug` using an API key provider
- **THEN** neither the log file nor the session file contains the key's value

#### Scenario: A command prints the environment
- **WHEN** the model runs `printenv` through the `bash` tool while `OPENAI_API_KEY` is set
- **THEN** the session file, the tool-output files and the NDJSON output show `[redacted]` in place of the key's value
```

In `openspec/changes/add-core-agent/specs/configuration/spec.md`:

Replace:

```markdown
- **THEN** harness reports the unknown key with its file and line

### Requirement: Widening project settings require workspace trust
The system SHALL apply project-level settings that widen what the agent may do (a `mode` that grants more than the effective global mode; a `max_steps` above the effective global limit; `model`; `allow` rules; `read_dirs`; provider definitions or `base_url` overrides; `[sandbox]` settings other than `allow_localhost = false` and `linux_git_protection = "required"`) only when the user has trusted the workspace with the current set of those settings. The effective global mode is the global config's `mode`, or else the default mode for the workspace, with modes ranked `plan` = `read-only` < `ask` < `auto` < `full-access`; the effective global limit is the global `max_steps`, or else the built-in default. Trust MUST be recorded in the data directory as a fingerprint of the widening settings; when they change, the workspace MUST be treated as untrusted until trusted again. On first interactive use of a workspace with such settings, the system MUST display them and ask whether to trust the workspace. Headless runs MUST ignore untrusted widening settings with a warning. Narrowing settings (`deny`, `confirm`, a `mode` or `max_steps` no wider than the effective global one, `allow_localhost = false`, and `linux_git_protection = "required"`) MUST always apply.

#### Scenario: Cloned repository redirects a provider
- **WHEN** a cloned repository's `.harness/config.toml` sets `providers.openai.base_url` to an unknown host and the workspace is not trusted
```

with:

```markdown
- **THEN** harness reports the unknown key with its file and line

### Requirement: Widening project settings require workspace trust
The system SHALL apply project-level settings that widen what the agent may do (a `mode` that grants more than the effective global mode; a `max_steps` above the effective global limit; `model`; `allow` rules; `read_dirs`; provider definitions or `base_url` overrides; model `[profiles]`; `[sandbox]` settings other than `allow_localhost = false` and `linux_git_protection = "required"`) only when the user has trusted the workspace with the current set of those settings. The effective global mode is the global config's `mode`, or else the default mode for the workspace, with modes ranked `plan` = `read-only` < `ask` < `auto` < `full-access`; the effective global limit is the global `max_steps`, or else the built-in default. Trust MUST be recorded in the data directory as a fingerprint of the widening settings; when they change, the workspace MUST be treated as untrusted until trusted again. On first interactive use of a workspace with such settings, the system MUST display them and ask whether to trust the workspace. Headless runs MUST ignore untrusted widening settings with a warning. Narrowing settings (`deny`, `confirm`, a `mode` or `max_steps` no wider than the effective global one, `allow_localhost = false`, and `linux_git_protection = "required"`) MUST always apply.

#### Scenario: Cloned repository redirects a provider
- **WHEN** a cloned repository's `.harness/config.toml` sets `providers.openai.base_url` to an unknown host and the workspace is not trusted
```

In `openspec/changes/add-core-agent/specs/cli-interface/spec.md`:

Replace:

```markdown
- **THEN** it reports the basic tier, the reason, and the commands that would enable the full tier

### Requirement: Management subcommands
The system SHALL provide `harness models`, `harness login <provider>`, `harness logout <provider>`, `harness auth add <provider>`, `harness auth use <provider> <profile>`, `harness trust [--yes] [--revoke]`, and `harness sandbox doctor`, with `--profile` accepted by `login`, `logout`, and `auth add`, and the flags `--model`, `--mode`, `-c`, and `--resume`.

#### Scenario: Help output
- **WHEN** the user runs `harness --help`
```

with:

```markdown
- **THEN** it reports the basic tier, the reason, and the commands that would enable the full tier

### Requirement: Management subcommands
The system SHALL provide `harness models`, `harness login <provider>`, `harness logout <provider>`, `harness auth add <provider>`, `harness auth use <provider> <profile>`, `harness trust [--yes] [--revoke]`, and `harness sandbox doctor`, with `--profile` accepted by `login`, `logout`, and `auth add`, `--device` accepted by `login`, and the flags `--model`, `--mode`, `-c`, `--resume`, and `--debug`. `harness auth add` MUST read the key from standard input. `--debug` MUST write the run's event stream to a log file in the state directory and print its path.

#### Scenario: Help output
- **WHEN** the user runs `harness --help`
```

- [ ] **Step 3: Point tasks.md at this plan**

In `openspec/changes/add-core-agent/tasks.md`:

Replace:

```markdown
- [x] 3.5 Checkpoints: shadow-repository snapshots, rewind of code/conversation/both, undo last rewind, degradation; verify the checkpoints scenarios as tests
- [x] 3.6 Compaction (automatic, `/compact`, overflow retry) and `/init`; verify the compaction scenarios with the mock provider (on-demand compaction is an API here; the interactive `/compact` command comes with the terminal UI in 5.7)

## 4. P4 Providers (plan written after P3)

- [ ] 4.1 OpenAI Responses adapter; verify with recorded SSE fixtures
- [ ] 4.2 Anthropic Messages adapter; verify with recorded SSE fixtures
```

with:

```markdown
- [x] 3.5 Checkpoints: shadow-repository snapshots, rewind of code/conversation/both, undo last rewind, degradation; verify the checkpoints scenarios as tests
- [x] 3.6 Compaction (automatic, `/compact`, overflow retry) and `/init`; verify the compaction scenarios with the mock provider (on-demand compaction is an API here; the interactive `/compact` command comes with the terminal UI in 5.7)

## 4. P4 Providers (`docs/superpowers/plans/2026-09-28-m1-p4-providers.md`)

- [ ] 4.1 OpenAI Responses adapter; verify with recorded SSE fixtures
- [ ] 4.2 Anthropic Messages adapter; verify with recorded SSE fixtures
```

- [ ] **Step 4: Validate**

Run: `openspec validate add-core-agent --strict`
Expected: `Change 'add-core-agent' is valid`

- [ ] **Step 5: Commit**

```bash
git add openspec/changes/add-core-agent
git commit -F - <<'EOF'
docs(spec): write the P4 refinements into the spec

The design records the built-in hosted providers, the stateless
Responses adapter, the Anthropic adapter's limits and cache
breakpoints, where consecutive messages are joined, how model
profiles resolve, the credential store and account profiles, the
ChatGPT OAuth endpoints and refresh rules, the Claude-credential
refusal, where secrets are redacted, --debug, and how the effective
context window is detected. Project profiles need workspace trust.

<trailer lines from the controller>
EOF
```

---

### Task 2: The OpenAI Responses adapter

**Files:**
- Create: `crates/harness-providers/src/sse.rs`, `crates/harness-providers/src/openai_responses.rs`, `crates/harness-providers/tests/openai_responses_parser.rs`, `crates/harness-providers/tests/openai_responses_http.rs`, and in `crates/harness-providers/tests/fixtures/openai-responses/`: `text.sse`, `tool_call.sse`, `reasoning.sse`, `incomplete.sse`, `failed.sse`
- Modify: `crates/harness-core/src/message.rs`, `crates/harness-core/src/agent.rs`, `crates/harness-core/src/compaction.rs`, `crates/harness-core/tests/agent.rs`, `crates/harness-config/src/config.rs`, `crates/harness-config/tests/config.rs`, `crates/harness-providers/src/lib.rs`, `crates/harness-providers/src/openai_chat.rs`, `crates/harness-providers/src/registry.rs`, `crates/harness-providers/tests/registry.rs`, `crates/harness-providers/tests/openai_chat_parser.rs`, `crates/harness-providers/tests/openai_chat_http.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces:
  - `harness_core::message::RequestOptions { max_output_tokens: Option<u64>, temperature: Option<f64>, reasoning_effort: Option<String> }` (`Default`, `PartialEq`); `ChatRequest` gains `options: RequestOptions` and derives `Default`.
  - `AgentConfig::request: RequestOptions`, sent with every request to the session's model (a slash command's model gets the defaults).
  - `harness_config::config::Protocol::OpenaiResponses`, written `"openai-responses"`.
  - `harness_providers::openai_responses::{request_body(req: &ChatRequest) -> Value, ResponsesStreamParser, OpenAiResponses::new(base_url, api_key: Option<String>)}`. The parser has `push(&mut self, data: &str) -> Result<Vec<ProviderEvent>, ProviderError>`, `finish(&mut self) -> Vec<ProviderEvent>` and `is_done(&self) -> bool`, like `ChatStreamParser`.
  - The private module `sse`: `trait EventParser { push, finish, is_done, may_end }`, `events(response: impl Future<Output = Result<reqwest::Response, ProviderError>> + Send + 'static, parser) -> ProviderStream`, `send(RequestBuilder)` and `http_error(Response) -> ProviderError`. Every adapter streams through it.
  - `registry::Builtin { name, protocol, base_url, key_env }` and `BUILTIN_PROVIDERS: [Builtin; 5]`, now with `openai` (Responses at `https://api.openai.com/v1`, `OPENAI_API_KEY`); `Resolved` gains `protocol: Protocol` and `base_url: String`.

The Responses protocol streams typed events (`response.output_text.delta`, `response.function_call_arguments.delta`, `response.output_item.done`, `response.completed`, `response.incomplete`, `response.failed`, `error`), each with a `data:` line whose JSON repeats its `type`, so the parser reads the data only. Function calls are keyed by `output_index` and emitted whole when the response ends; `incomplete_details.reason = "max_output_tokens"` is a `Length` finish. Tools go with `"strict": false`, because strict schemas must list every property as required and harness's tools have optional ones.

- [ ] **Step 1: Write the failing tests**

The fixtures follow OpenAI's documented streaming examples.

Create `crates/harness-providers/tests/fixtures/openai-responses/text.sse`:

```text
event: response.created
data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_1","object":"response","status":"in_progress","model":"gpt-5","output":[]}}

event: response.in_progress
data: {"type":"response.in_progress","sequence_number":1,"response":{"id":"resp_1","object":"response","status":"in_progress","model":"gpt-5","output":[]}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"msg_1","type":"message","status":"in_progress","role":"assistant","content":[]}}

event: response.content_part.added
data: {"type":"response.content_part.added","sequence_number":3,"item_id":"msg_1","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}

event: response.output_text.delta
data: {"type":"response.output_text.delta","sequence_number":4,"item_id":"msg_1","output_index":0,"content_index":0,"delta":"Hello"}

event: response.output_text.delta
data: {"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_1","output_index":0,"content_index":0,"delta":", world"}

event: response.output_text.done
data: {"type":"response.output_text.done","sequence_number":6,"item_id":"msg_1","output_index":0,"content_index":0,"text":"Hello, world"}

event: response.content_part.done
data: {"type":"response.content_part.done","sequence_number":7,"item_id":"msg_1","output_index":0,"content_index":0,"part":{"type":"output_text","text":"Hello, world","annotations":[]}}

event: response.output_item.done
data: {"type":"response.output_item.done","sequence_number":8,"output_index":0,"item":{"id":"msg_1","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"Hello, world","annotations":[]}]}}

event: response.completed
data: {"type":"response.completed","sequence_number":9,"response":{"id":"resp_1","object":"response","status":"completed","model":"gpt-5","output":[{"id":"msg_1","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"Hello, world","annotations":[]}]}],"usage":{"input_tokens":36,"input_tokens_details":{"cached_tokens":12},"output_tokens":87,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":123}}}

```

Create `crates/harness-providers/tests/fixtures/openai-responses/tool_call.sse`:

```text
event: response.created
data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_2","object":"response","status":"in_progress","model":"gpt-5","output":[]}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"id":"fc_1","type":"function_call","status":"in_progress","arguments":"","call_id":"call_abc","name":"read"}}

event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","sequence_number":2,"item_id":"fc_1","output_index":0,"delta":"{\"path\":"}

event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","sequence_number":3,"item_id":"fc_1","output_index":0,"delta":"\"src/lib.rs\"}"}

event: response.function_call_arguments.done
data: {"type":"response.function_call_arguments.done","sequence_number":4,"item_id":"fc_1","output_index":0,"arguments":"{\"path\":\"src/lib.rs\"}"}

event: response.output_item.done
data: {"type":"response.output_item.done","sequence_number":5,"output_index":0,"item":{"id":"fc_1","type":"function_call","status":"completed","arguments":"{\"path\":\"src/lib.rs\"}","call_id":"call_abc","name":"read"}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":6,"output_index":1,"item":{"id":"fc_2","type":"function_call","status":"in_progress","arguments":"","call_id":"call_def","name":"glob"}}

event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","sequence_number":7,"item_id":"fc_2","output_index":1,"delta":"{\"pattern\":\"*.md\"}"}

event: response.output_item.done
data: {"type":"response.output_item.done","sequence_number":8,"output_index":1,"item":{"id":"fc_2","type":"function_call","status":"completed","arguments":"{\"pattern\":\"*.md\"}","call_id":"call_def","name":"glob"}}

event: response.completed
data: {"type":"response.completed","sequence_number":9,"response":{"id":"resp_2","object":"response","status":"completed","model":"gpt-5","usage":{"input_tokens":120,"input_tokens_details":{"cached_tokens":0},"output_tokens":40,"output_tokens_details":{"reasoning_tokens":16},"total_tokens":160}}}

```

Create `crates/harness-providers/tests/fixtures/openai-responses/reasoning.sse`:

```text
event: response.created
data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_3","object":"response","status":"in_progress","model":"gpt-5","output":[]}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[]}}

event: response.reasoning_summary_part.added
data: {"type":"response.reasoning_summary_part.added","sequence_number":2,"item_id":"rs_1","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}

event: response.reasoning_summary_text.delta
data: {"type":"response.reasoning_summary_text.delta","sequence_number":3,"item_id":"rs_1","output_index":0,"summary_index":0,"delta":"**Checking the tests**"}

event: response.reasoning_summary_text.done
data: {"type":"response.reasoning_summary_text.done","sequence_number":4,"item_id":"rs_1","output_index":0,"summary_index":0,"text":"**Checking the tests**"}

event: response.output_item.done
data: {"type":"response.output_item.done","sequence_number":5,"output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[{"type":"summary_text","text":"**Checking the tests**"}]}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":6,"output_index":1,"item":{"id":"msg_3","type":"message","status":"in_progress","role":"assistant","content":[]}}

event: response.output_text.delta
data: {"type":"response.output_text.delta","sequence_number":7,"item_id":"msg_3","output_index":1,"content_index":0,"delta":"All green."}

event: response.output_item.done
data: {"type":"response.output_item.done","sequence_number":8,"output_index":1,"item":{"id":"msg_3","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"All green.","annotations":[]}]}}

event: response.completed
data: {"type":"response.completed","sequence_number":9,"response":{"id":"resp_3","object":"response","status":"completed","model":"gpt-5","usage":{"input_tokens":50,"input_tokens_details":{"cached_tokens":0},"output_tokens":30,"output_tokens_details":{"reasoning_tokens":24},"total_tokens":80}}}

```

Create `crates/harness-providers/tests/fixtures/openai-responses/incomplete.sse`:

```text
event: response.created
data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_4","object":"response","status":"in_progress","model":"gpt-5","output":[]}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"id":"fc_9","type":"function_call","status":"in_progress","arguments":"","call_id":"call_cut","name":"write"}}

event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","sequence_number":2,"item_id":"fc_9","output_index":0,"delta":"{\"path\":\"big.txt\",\"content\":\"aaaa"}

event: response.incomplete
data: {"type":"response.incomplete","sequence_number":3,"response":{"id":"resp_4","object":"response","status":"incomplete","model":"gpt-5","incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":0},"output_tokens":16,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":26}}}

```

Create `crates/harness-providers/tests/fixtures/openai-responses/failed.sse`:

```text
event: response.created
data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_5","object":"response","status":"in_progress","model":"gpt-5","output":[]}}

event: response.failed
data: {"type":"response.failed","sequence_number":1,"response":{"id":"resp_5","object":"response","status":"failed","model":"gpt-5","error":{"code":"context_length_exceeded","message":"Your input exceeds the context window of this model. Please adjust your input and try again."}}}

```

Create `crates/harness-providers/tests/openai_responses_parser.rs`:

```rust
//! The Responses stream parser and request body, against fixtures built from the documented
//! streaming examples (`tests/fixtures/openai-responses/`).

use harness_core::message::{ChatRequest, Message, RequestOptions, ToolCall, ToolSpec, Usage};
use harness_core::provider::{FinishReason, ProviderError, ProviderEvent};
use harness_providers::openai_responses::{ResponsesStreamParser, request_body};
use serde_json::json;

/// The `data:` payloads of a fixture, in order.
fn payloads(name: &str) -> Vec<String> {
    let path = format!(
        "{}/tests/fixtures/openai-responses/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("data: ").map(String::from))
        .collect()
}

/// Feeds a fixture to a parser, as the provider does: until the parser is done, then whatever
/// `finish` still has.
fn parse(name: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
    let mut parser = ResponsesStreamParser::default();
    let mut events = Vec::new();
    for data in payloads(name) {
        events.extend(parser.push(&data)?);
        if parser.is_done() {
            break;
        }
    }
    events.extend(parser.finish());
    Ok(events)
}

#[test]
fn text_arrives_as_deltas_with_usage_and_a_stop() {
    assert_eq!(
        parse("text.sse").unwrap(),
        vec![
            ProviderEvent::TextDelta("Hello".into()),
            ProviderEvent::TextDelta(", world".into()),
            ProviderEvent::Usage(Usage {
                input_tokens: 36,
                output_tokens: 87,
                cached_tokens: 12,
            }),
            ProviderEvent::Finished(FinishReason::Stop),
        ]
    );
}

#[test]
fn function_calls_are_emitted_whole_in_output_order() {
    let events = parse("tool_call.sse").unwrap();
    assert_eq!(
        events[1..],
        [
            ProviderEvent::ToolCall(ToolCall {
                id: "call_abc".into(),
                name: "read".into(),
                arguments: r#"{"path":"src/lib.rs"}"#.into(),
            }),
            ProviderEvent::ToolCall(ToolCall {
                id: "call_def".into(),
                name: "glob".into(),
                arguments: r#"{"pattern":"*.md"}"#.into(),
            }),
            ProviderEvent::Finished(FinishReason::ToolCalls),
        ]
    );
    assert!(matches!(events[0], ProviderEvent::Usage(_)));
}

#[test]
fn reasoning_summaries_are_reasoning_deltas() {
    let events = parse("reasoning.sse").unwrap();
    assert_eq!(
        events[..2],
        [
            ProviderEvent::ReasoningDelta("**Checking the tests**".into()),
            ProviderEvent::TextDelta("All green.".into()),
        ]
    );
    assert_eq!(
        events.last(),
        Some(&ProviderEvent::Finished(FinishReason::Stop))
    );
}

#[test]
fn a_reply_stopped_by_the_output_limit_finishes_with_length() {
    let events = parse("incomplete.sse").unwrap();
    // The cut-off call is still reported, whole as far as it goes: the agent decides what to do.
    assert!(events.contains(&ProviderEvent::ToolCall(ToolCall {
        id: "call_cut".into(),
        name: "write".into(),
        arguments: r#"{"path":"big.txt","content":"aaaa"#.into(),
    })));
    assert_eq!(
        events.last(),
        Some(&ProviderEvent::Finished(FinishReason::Length))
    );
}

#[test]
fn a_failed_response_is_an_error_that_names_its_code() {
    let error = parse("failed.sse").unwrap_err();
    assert!(matches!(error, ProviderError::InStream(_)), "{error:?}");
    assert!(
        error.to_string().contains("context_length_exceeded"),
        "{error}"
    );
    assert!(error.is_context_overflow());
}

#[test]
fn server_errors_and_rate_limits_in_the_stream_can_be_retried() {
    for (code, status) in [("server_error", 500), ("rate_limit_exceeded", 429)] {
        let mut parser = ResponsesStreamParser::default();
        let data = json!({"type": "response.failed", "response": {"status": "failed",
            "error": {"code": code, "message": "try again"}}});
        let error = parser.push(&data.to_string()).unwrap_err();
        assert!(
            matches!(error, ProviderError::Http { status: s, .. } if s == status),
            "{error:?}"
        );
        assert!(error.is_retryable());
    }
    let mut parser = ResponsesStreamParser::default();
    let data = json!({"type": "error", "code": "invalid_prompt", "message": "bad prompt"});
    let error = parser.push(&data.to_string()).unwrap_err();
    assert_eq!(
        error,
        ProviderError::InStream("invalid_prompt: bad prompt".into())
    );
}

#[test]
fn unparseable_events_are_protocol_errors() {
    let mut parser = ResponsesStreamParser::default();
    assert!(matches!(
        parser.push("{not json"),
        Err(ProviderError::Protocol(_))
    ));
}

#[test]
fn finish_is_idempotent() {
    let mut parser = ResponsesStreamParser::default();
    for data in payloads("text.sse") {
        parser.push(&data).unwrap();
    }
    assert!(parser.is_done());
    assert!(parser.finish().is_empty());
}

fn conversation() -> ChatRequest {
    ChatRequest {
        model: "gpt-5".into(),
        system: "be brief".into(),
        messages: vec![
            Message::User {
                content: "hi".into(),
            },
            Message::Assistant {
                content: "Looking.".into(),
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"a"}"#.into(),
                }],
                model: "openai/gpt-5".into(),
            },
            Message::Tool {
                call_id: "call_1".into(),
                content: "data".into(),
                is_error: false,
            },
        ],
        tools: vec![ToolSpec {
            name: "read".into(),
            description: "Read".into(),
            parameters: json!({"type": "object"}),
        }],
        options: RequestOptions::default(),
    }
}

#[test]
fn the_request_carries_the_whole_conversation_without_server_state() {
    let body = request_body(&conversation());
    assert_eq!(body["model"], "gpt-5");
    assert_eq!(body["instructions"], "be brief");
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(
        body["input"],
        json!([
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Looking."}]},
            {"type": "function_call", "call_id": "call_1", "name": "read", "arguments": "{\"path\":\"a\"}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "data"},
        ])
    );
    // Strict schemas would require every property: harness's tools have optional ones.
    assert_eq!(
        body["tools"],
        json!([{"type": "function", "name": "read", "description": "Read",
            "parameters": {"type": "object"}, "strict": false}])
    );
    assert_eq!(body["tool_choice"], "auto");
    for absent in [
        "max_output_tokens",
        "temperature",
        "reasoning",
        "previous_response_id",
    ] {
        assert!(body.get(absent).is_none(), "{absent} should be absent");
    }
}

#[test]
fn profile_options_reach_the_request() {
    let mut request = conversation();
    request.options = RequestOptions {
        max_output_tokens: Some(4096),
        temperature: Some(0.2),
        reasoning_effort: Some("high".into()),
    };
    let body = request_body(&request);
    assert_eq!(body["max_output_tokens"], 4096);
    assert_eq!(body["temperature"], 0.2);
    assert_eq!(
        body["reasoning"],
        json!({"effort": "high", "summary": "auto"})
    );
}

#[test]
fn an_assistant_message_with_only_tool_calls_sends_no_empty_text() {
    let mut request = conversation();
    if let Message::Assistant { content, .. } = &mut request.messages[1] {
        content.clear();
    }
    let body = request_body(&request);
    assert_eq!(body["input"][1]["type"], "function_call");
    assert!(request_body(&ChatRequest::default()).get("tools").is_none());
}
```

Create `crates/harness-providers/tests/openai_responses_http.rs`:

```rust
//! The Responses provider over HTTP, against a mock server replaying the fixtures.

use futures::StreamExt;
use harness_core::message::{ChatRequest, Message};
use harness_core::provider::{FinishReason, Provider, ProviderError, ProviderEvent};
use harness_providers::openai_responses::OpenAiResponses;
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/openai-responses/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn sse(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "gpt-5".into(),
        system: "s".into(),
        messages: vec![Message::User {
            content: "hi".into(),
        }],
        ..ChatRequest::default()
    }
}

#[tokio::test]
async fn streams_a_reply_with_the_api_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer sk-test"))
        .and(body_partial_json(
            json!({"model": "gpt-5", "instructions": "s", "store": false, "stream": true}),
        ))
        .respond_with(sse(fixture("text.sse")))
        .mount(&server)
        .await;
    let provider = OpenAiResponses::new(format!("{}/v1", server.uri()), Some("sk-test".into()));
    let events: Vec<_> = provider.stream(request()).collect().await;
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            Ok(ProviderEvent::TextDelta(t)) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello, world");
    assert_eq!(
        events.last(),
        Some(&Ok(ProviderEvent::Finished(FinishReason::Stop)))
    );
}

#[tokio::test]
async fn tool_calls_come_through() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(fixture("tool_call.sse")))
        .mount(&server)
        .await;
    let provider = OpenAiResponses::new(format!("{}/v1", server.uri()), None);
    let events: Vec<_> = provider.stream(request()).collect().await;
    let names: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            Ok(ProviderEvent::ToolCall(call)) => Some(call.name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(names, ["read", "glob"]);
}

#[tokio::test]
async fn a_context_overflow_response_is_recognised() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"message":"Your input exceeds the context window of this model. Please adjust your input and try again.","type":"invalid_request_error","param":"input","code":"context_length_exceeded"}}"#,
        ))
        .mount(&server)
        .await;
    let provider = OpenAiResponses::new(format!("{}/v1", server.uri()), None);
    let first = provider.stream(request()).next().await.unwrap();
    let error = first.unwrap_err();
    assert!(
        matches!(error, ProviderError::Http { status: 400, .. }),
        "{error:?}"
    );
    assert!(error.is_context_overflow());
}

// A connection that drops before `response.completed` is not a finished reply.
#[tokio::test]
async fn a_stream_cut_before_completion_is_a_network_error() {
    let server = MockServer::start().await;
    let cut: String = fixture("text.sse")
        .split("event: response.completed")
        .next()
        .unwrap()
        .to_string();
    Mock::given(method("POST"))
        .respond_with(sse(cut))
        .mount(&server)
        .await;
    let provider = OpenAiResponses::new(format!("{}/v1", server.uri()), None);
    let events: Vec<_> = provider.stream(request()).collect().await;
    assert!(
        matches!(events.last(), Some(Err(ProviderError::Network(_)))),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_failed_response_ends_the_stream_with_its_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(fixture("failed.sse")))
        .mount(&server)
        .await;
    let provider = OpenAiResponses::new(format!("{}/v1", server.uri()), None);
    let events: Vec<_> = provider.stream(request()).collect().await;
    assert!(
        matches!(events.last(), Some(Err(e)) if e.is_context_overflow()),
        "{events:?}"
    );
}
```

Append to `crates/harness-providers/tests/registry.rs`, after a blank line:

```rust
#[test]
fn builtin_openai_speaks_the_responses_protocol_with_its_key() {
    let r = resolve(
        "openai/gpt-5",
        &BTreeMap::new(),
        env(&[("OPENAI_API_KEY", "k")]),
    )
    .unwrap();
    assert_eq!(r.model, "gpt-5");
    assert_eq!(r.protocol, Protocol::OpenaiResponses);
    assert_eq!(r.base_url, "https://api.openai.com/v1");
    assert_eq!(
        resolve("openai/gpt-5", &BTreeMap::new(), env(&[])).err(),
        Some(ResolveError::MissingKey {
            provider: "openai".into(),
            var: "OPENAI_API_KEY".into()
        })
    );
    let found = configured_endpoints(&BTreeMap::new(), env(&[("OPENAI_API_KEY", "k")]));
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].provider, "openai");
}

#[test]
fn configured_providers_may_speak_the_responses_protocol() {
    let mut providers = custom("azure", "https://x.example/openai/v1", Some("AZ_KEY"));
    providers.get_mut("azure").unwrap().protocol = Protocol::OpenaiResponses;
    let r = resolve("azure/gpt-5", &providers, env(&[("AZ_KEY", "k")])).unwrap();
    assert_eq!(r.protocol, Protocol::OpenaiResponses);
    assert_eq!(r.base_url, "https://x.example/openai/v1");
}
```

Append to `crates/harness-config/tests/config.rs`, after a blank line:

```rust
#[test]
fn providers_may_use_the_responses_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    std::fs::write(
        &file,
        "[providers.oa]\nprotocol = \"openai-responses\"\nbase_url = \"https://x.example/v1\"\n",
    )
    .unwrap();
    let parsed = config::parse_file(&file).unwrap().unwrap();
    assert_eq!(
        parsed.providers["oa"].protocol,
        config::Protocol::OpenaiResponses
    );
}
```

Append to `crates/harness-core/tests/agent.rs`, after a blank line:

```rust
#[tokio::test]
async fn the_configured_request_options_reach_the_provider() {
    use harness_core::message::RequestOptions;
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::text("ok")]);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    let options = RequestOptions {
        max_output_tokens: Some(1000),
        temperature: Some(0.3),
        reasoning_effort: Some("low".into()),
    };
    agent.config_mut().request = options.clone();
    run(&mut agent, "hi").await;
    assert_eq!(provider.requests()[0].options, options);
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p harness-providers --test openai_responses_parser --test openai_responses_http --test registry; cargo test -p harness-config --test config; cargo test -p harness-core --test agent`
Expected: FAIL to compile. `openai_responses_parser` and `openai_responses_http`: ``unresolved import `harness_providers::openai_responses` `` and ``no associated function or constant named `default` found for struct `ChatRequest` ``; the parser also ``unresolved import `harness_core::message::RequestOptions` ``, ``struct `ChatRequest` has no field named `options` ``. `registry`: ``no variant, associated function, or constant named `OpenaiResponses` found for enum `Protocol` `` and ``no field `protocol` `` / ``no field `base_url` on type `Resolved` ``. `config`: ``no variant … named `OpenaiResponses` found for enum `harness_config::config::Protocol` ``. `agent`: ``unresolved import `harness_core::message::RequestOptions` ``, ``no field `options` on type `ChatRequest` ``, ``no field `request` on type `&mut AgentConfig` ``.

- [ ] **Step 3: Add request options to the core**

In `crates/harness-core/src/message.rs`:

Replace:

```rust
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

with:

```rust
    pub cached_tokens: u64,
}

/// Settings a model profile gives each request. `None` leaves the provider's default.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RequestOptions {
    pub max_output_tokens: Option<u64>,
    pub temperature: Option<f64>,
    /// For models that reason: `minimal`, `low`, `medium` or `high`, as the provider names it.
    pub reasoning_effort: Option<String>,
}

/// Everything a provider needs for one model call.
#[derive(Debug, Clone, Default)]
pub struct ChatRequest {
    /// Model name as the provider knows it (without the `<provider>/` prefix).
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub options: RequestOptions,
}
```

In `crates/harness-core/src/agent.rs`:

Replace (1 of 4):

```rust
    checkpoint::{CheckpointError, Checkpoints},
    compaction::{self, CompactionConfig},
    event::{AgentEvent, ErrorKind, TurnEndReason},
    message::{ChatRequest, Message, ToolCall, Usage},
    output::{DEFAULT_OUTPUT_LIMIT, limit_output},
    permission::{Action, Decision, FsAccess, Mode, PermissionPolicy},
    provider::{FinishReason, Provider, ProviderError, ProviderEvent},
```

with:

```rust
    checkpoint::{CheckpointError, Checkpoints},
    compaction::{self, CompactionConfig},
    event::{AgentEvent, ErrorKind, TurnEndReason},
    message::{ChatRequest, Message, RequestOptions, ToolCall, Usage},
    output::{DEFAULT_OUTPUT_LIMIT, limit_output},
    permission::{Action, Decision, FsAccess, Mode, PermissionPolicy},
    provider::{FinishReason, Provider, ProviderError, ProviderEvent},
```

Replace (2 of 4):

```rust
    /// The model's context window in tokens.
    pub context_window: u64,
    pub compaction: CompactionConfig,
}

impl AgentConfig {
```

with:

```rust
    /// The model's context window in tokens.
    pub context_window: u64,
    pub compaction: CompactionConfig,
    /// Output limit, temperature and reasoning effort for every request to the session's model.
    pub request: RequestOptions,
}

impl AgentConfig {
```

Replace (3 of 4):

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
            request: RequestOptions::default(),
        }
    }
}
```

Replace (4 of 4):

```rust
        reply: &mut ModelReply,
        events: &UnboundedSender<AgentEvent>,
    ) -> Result<(), ProviderError> {
        let (provider, model) = match &self.turn_model {
            Some(turn) => (&turn.provider, turn.name.clone()),
            None => (&self.provider, self.config.model_name.clone()),
        };
        let request = ChatRequest {
            model,
            system: self.config.system_prompt.clone(),
            messages: request_messages(&self.history),
            tools: self.tools.specs(),
        };
        let mut stream = provider.stream(request);
        while let Some(item) = stream.next().await {
```

with:

```rust
        reply: &mut ModelReply,
        events: &UnboundedSender<AgentEvent>,
    ) -> Result<(), ProviderError> {
        // A slash command's model gets the provider's defaults: the options are the session
        // model's.
        let (provider, model, options) = match &self.turn_model {
            Some(turn) => (&turn.provider, turn.name.clone(), RequestOptions::default()),
            None => (
                &self.provider,
                self.config.model_name.clone(),
                self.config.request.clone(),
            ),
        };
        let request = ChatRequest {
            model,
            system: self.config.system_prompt.clone(),
            messages: request_messages(&self.history),
            tools: self.tools.specs(),
            options,
        };
        let mut stream = provider.stream(request);
        while let Some(item) = stream.next().await {
```

In `crates/harness-core/src/compaction.rs`, the summary request takes the provider's defaults:

Replace:

```rust
        system: SUMMARY_SYSTEM.to_string(),
        messages: vec![Message::User { content }],
        tools: Vec::new(),
    }
}

```

with:

```rust
        system: SUMMARY_SYSTEM.to_string(),
        messages: vec![Message::User { content }],
        tools: Vec::new(),
        ..ChatRequest::default()
    }
}

```

The chat adapter's tests build `ChatRequest`s field by field; they take the new field's default. In `crates/harness-providers/tests/openai_chat_parser.rs`:

Replace (1 of 2):

```rust
            description: "Read".into(),
            parameters: json!({"type": "object"}),
        }],
    };
    let body = request_body(&req);
    assert_eq!(body["model"], "qwen3:14b");
```

with:

```rust
            description: "Read".into(),
            parameters: json!({"type": "object"}),
        }],
        ..ChatRequest::default()
    };
    let body = request_body(&req);
    assert_eq!(body["model"], "qwen3:14b");
```

Replace (2 of 2):

```rust
        system: String::new(),
        messages: vec![],
        tools: vec![],
    };
    assert!(request_body(&req).get("tools").is_none());
}
```

with:

```rust
        system: String::new(),
        messages: vec![],
        tools: vec![],
        ..ChatRequest::default()
    };
    assert!(request_body(&req).get("tools").is_none());
}
```

In `crates/harness-providers/tests/openai_chat_http.rs`:

Replace:

```rust
        system: "s".into(),
        messages: vec![],
        tools: vec![],
    }
}

```

with:

```rust
        system: "s".into(),
        messages: vec![],
        tools: vec![],
        ..ChatRequest::default()
    }
}

```

- [ ] **Step 4: Add the protocol and the shared stream reader**

In `crates/harness-config/src/config.rs`:

Replace:

```rust

use crate::trust::TrustStore;

/// Wire protocol spoken by a configured provider. P4 adds `openai-responses` and `anthropic-messages`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    OpenaiChat,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
```

with:

```rust

use crate::trust::TrustStore;

/// Wire protocol spoken by a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    /// `POST /chat/completions`: Ollama, LM Studio, llama.cpp, OpenRouter and most others.
    OpenaiChat,
    /// `POST /responses`: OpenAI API keys and ChatGPT sign-in.
    OpenaiResponses,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
```

Create `crates/harness-providers/src/sse.rs`:

```rust
//! What the adapters share: sending a request, turning an error response into a
//! [`ProviderError`], and reading a success response's server-sent events through a parser.

use std::{future::Future, time::Duration};

use eventsource_stream::Eventsource;
use futures::StreamExt;
use harness_core::provider::{ProviderError, ProviderEvent, ProviderStream};

/// Turns the `data:` payloads of one response's server-sent events into [`ProviderEvent`]s.
pub trait EventParser: Send + 'static {
    /// The events one payload yields.
    fn push(&mut self, data: &str) -> Result<Vec<ProviderEvent>, ProviderError>;
    /// What is still buffered, then `Finished`. Calling it again yields nothing.
    fn finish(&mut self) -> Vec<ProviderEvent>;
    /// Whether the reply is complete, so the rest of the stream need not be read.
    fn is_done(&self) -> bool;
    /// Whether the stream may end here without the reply having been cut off.
    fn may_end(&self) -> bool {
        self.is_done()
    }
}

/// Awaits `response`, then streams its events through `parser`. An error status ends the stream
/// with [`ProviderError::Http`]; a stream that ends before the reply finished, with
/// [`ProviderError::Network`].
pub fn events<P: EventParser>(
    response: impl Future<Output = Result<reqwest::Response, ProviderError>> + Send + 'static,
    mut parser: P,
) -> ProviderStream {
    // The if/else keeps `response` used within one branch: `http_error` takes it by value.
    Box::pin(async_stream::try_stream! {
        let response = response.await?;
        if response.status().is_success() {
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
            // The connection closed mid-reply: that must not pass for a normal completion.
            if !parser.may_end() {
                Err::<(), ProviderError>(ProviderError::Network(
                    "stream ended before the response finished".into(),
                ))?;
            }
            for item in parser.finish() {
                yield item;
            }
        } else {
            // `?` on an `Err` ends the stream with this error.
            Err::<(), ProviderError>(http_error(response).await)?;
        }
    })
}

/// Sends `request`, mapping a failure to connect or send to [`ProviderError::Network`].
pub async fn send(request: reqwest::RequestBuilder) -> Result<reqwest::Response, ProviderError> {
    request
        .send()
        .await
        .map_err(|e| ProviderError::Network(e.to_string()))
}

/// The error an unsuccessful response stands for: its status, body and `Retry-After` in seconds.
pub async fn http_error(response: reqwest::Response) -> ProviderError {
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let body = response.text().await.unwrap_or_default();
    ProviderError::Http {
        status,
        body,
        retry_after,
    }
}
```

The chat adapter moves onto it; its parser keeps its own methods, which its tests call. In `crates/harness-providers/src/openai_chat.rs`:

Replace (1 of 3):

```rust
use std::{collections::BTreeMap, time::Duration};

use eventsource_stream::Eventsource;
use futures::StreamExt;
use harness_core::{
    message::{ChatRequest, Message, ToolCall, Usage},
    provider::{FinishReason, Provider, ProviderError, ProviderEvent, ProviderStream},
};
use serde_json::{Value, json};

/// Builds a streaming Chat Completions request body.
pub fn request_body(req: &ChatRequest) -> Value {
    let mut messages = vec![json!({"role": "system", "content": req.system})];
```

with:

```rust
use std::collections::BTreeMap;

use harness_core::{
    message::{ChatRequest, Message, ToolCall, Usage},
    provider::{FinishReason, Provider, ProviderError, ProviderEvent, ProviderStream},
};
use serde_json::{Value, json};

use crate::sse::{self, EventParser};

/// Builds a streaming Chat Completions request body.
pub fn request_body(req: &ChatRequest) -> Value {
    let mut messages = vec![json!({"role": "system", "content": req.system})];
```

Replace (2 of 3):

```rust
    }
}

impl Provider for OpenAiChat {
    fn stream(&self, request: ChatRequest) -> ProviderStream {
        let mut http = self
```

with:

```rust
    }
}

impl EventParser for ChatStreamParser {
    fn push(&mut self, data: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
        ChatStreamParser::push(self, data)
    }

    fn finish(&mut self) -> Vec<ProviderEvent> {
        ChatStreamParser::finish(self)
    }

    fn is_done(&self) -> bool {
        ChatStreamParser::is_done(self)
    }

    /// Some servers omit the trailing `[DONE]`: a `finish_reason` already ends the reply.
    fn may_end(&self) -> bool {
        self.is_done() || self.saw_finish_reason()
    }
}

impl Provider for OpenAiChat {
    fn stream(&self, request: ChatRequest) -> ProviderStream {
        let mut http = self
```

Replace (3 of 3):

```rust
        if let Some(key) = &self.api_key {
            http = http.bearer_auth(key);
        }
        // The if/else keeps `response` used entirely within one branch: `Response::text` takes
        // `self` by value, so reading the error body and then still using `response` for the
        // success-path byte stream (as one flat sequence with an early-return `?` in between)
        // does not borrow-check, even though the `?` diverges before the byte-stream line runs.
        Box::pin(async_stream::try_stream! {
            let response = http.send().await.map_err(|e| ProviderError::Network(e.to_string()))?;
            let status = response.status();
            if status.is_success() {
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
                // The byte stream ended without `[DONE]`. That's fine if we already saw a
                // `finish_reason` (some servers omit the trailing `[DONE]`), but otherwise the
                // connection dropped mid-reply and must not be mistaken for a normal completion.
                if !parser.is_done() && !parser.saw_finish_reason() {
                    Err::<(), ProviderError>(ProviderError::Network(
                        "stream ended before the response finished".into(),
                    ))?;
                }
                for item in parser.finish() {
                    yield item;
                }
            } else {
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
        })
    }
}
```

with:

```rust
        if let Some(key) = &self.api_key {
            http = http.bearer_auth(key);
        }
        sse::events(sse::send(http), ChatStreamParser::default())
    }
}
```

- [ ] **Step 5: Add the Responses adapter and the `openai` provider**

Create `crates/harness-providers/src/openai_responses.rs`:

```rust
//! The OpenAI Responses protocol (`POST /responses`, server-sent events): OpenAI API keys, and
//! ChatGPT sign-in. Requests are stateless: each carries the whole conversation with
//! `store: false`, so nothing on the server has to outlive a model switch.

use std::collections::BTreeMap;

use harness_core::{
    message::{ChatRequest, Message, ToolCall, Usage},
    provider::{FinishReason, Provider, ProviderError, ProviderEvent, ProviderStream},
};
use serde_json::{Value, json};

use crate::sse::{self, EventParser};

/// Builds a streaming Responses request body.
pub fn request_body(req: &ChatRequest) -> Value {
    let mut input = Vec::new();
    for message in &req.messages {
        match message {
            Message::User { content } => input.push(json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": content}],
            })),
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => {
                if !content.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": content}],
                    }));
                }
                for call in tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": call.arguments,
                    }));
                }
            }
            Message::Tool {
                call_id, content, ..
            } => input.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": content,
            })),
        }
    }
    let mut body = json!({
        "model": req.model,
        "instructions": req.system,
        "input": input,
        "stream": true,
        "store": false,
    });
    if !req.tools.is_empty() {
        // Strict schemas would have to list every property as required.
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                        "strict": false,
                    })
                })
                .collect(),
        );
        body["tool_choice"] = json!("auto");
    }
    if let Some(tokens) = req.options.max_output_tokens {
        body["max_output_tokens"] = json!(tokens);
    }
    if let Some(temperature) = req.options.temperature {
        body["temperature"] = json!(temperature);
    }
    if let Some(effort) = &req.options.reasoning_effort {
        body["reasoning"] = json!({"effort": effort, "summary": "auto"});
    }
    body
}

#[derive(Debug, Default)]
struct PartialCall {
    call_id: String,
    name: String,
    arguments: String,
}

/// Turns Responses stream events into [`ProviderEvent`]s. Function calls are buffered by output
/// index and emitted whole, in output order, when the response ends.
#[derive(Debug, Default)]
pub struct ResponsesStreamParser {
    calls: BTreeMap<u64, PartialCall>,
    finish: Option<FinishReason>,
    done: bool,
}

impl ResponsesStreamParser {
    pub fn push(&mut self, data: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
        let event: Value = serde_json::from_str(data)
            .map_err(|e| ProviderError::Protocol(format!("{e} in event: {data}")))?;
        let mut out = Vec::new();
        let delta = || event["delta"].as_str().filter(|d| !d.is_empty());
        match event["type"].as_str().unwrap_or_default() {
            "response.output_text.delta" => {
                if let Some(text) = delta() {
                    out.push(ProviderEvent::TextDelta(text.to_string()));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(text) = delta() {
                    out.push(ProviderEvent::ReasoningDelta(text.to_string()));
                }
            }
            "response.output_item.added" | "response.output_item.done"
                if event["item"]["type"] == "function_call" =>
            {
                let item = &event["item"];
                let call = self.call(&event);
                if let Some(id) = item["call_id"].as_str() {
                    call.call_id = id.to_string();
                }
                if let Some(name) = item["name"].as_str() {
                    call.name = name.to_string();
                }
                // `done` carries the complete arguments; `added` usually none yet.
                if let Some(arguments) = item["arguments"].as_str().filter(|a| !a.is_empty()) {
                    call.arguments = arguments.to_string();
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(fragment) = event["delta"].as_str() {
                    self.call(&event).arguments.push_str(fragment);
                }
            }
            "response.function_call_arguments.done" => {
                if let Some(arguments) = event["arguments"].as_str() {
                    self.call(&event).arguments = arguments.to_string();
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = &event["response"];
                if let Some(usage) = response.get("usage").filter(|u| u.is_object()) {
                    out.push(ProviderEvent::Usage(Usage {
                        input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
                        output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
                        cached_tokens: usage["input_tokens_details"]["cached_tokens"]
                            .as_u64()
                            .unwrap_or(0),
                    }));
                }
                self.finish = Some(match response["incomplete_details"]["reason"].as_str() {
                    Some("max_output_tokens") => FinishReason::Length,
                    Some(other) => FinishReason::Other(other.to_string()),
                    None if !self.calls.is_empty() => FinishReason::ToolCalls,
                    None => FinishReason::Stop,
                });
                out.extend(self.finish());
            }
            "response.failed" => return Err(stream_error(&event["response"]["error"])),
            "error" => return Err(stream_error(&event)),
            _ => {}
        }
        Ok(out)
    }

    /// The call at the event's output index.
    fn call(&mut self, event: &Value) -> &mut PartialCall {
        let index = event["output_index"]
            .as_u64()
            .unwrap_or_else(|| self.calls.keys().last().copied().unwrap_or(0));
        self.calls.entry(index).or_default()
    }

    /// Emits buffered calls followed by `Finished`. Calling it again yields nothing.
    pub fn finish(&mut self) -> Vec<ProviderEvent> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        let mut out: Vec<ProviderEvent> = std::mem::take(&mut self.calls)
            .into_iter()
            .map(|(index, call)| {
                ProviderEvent::ToolCall(ToolCall {
                    id: if call.call_id.is_empty() {
                        format!("call_{index}")
                    } else {
                        call.call_id
                    },
                    name: call.name,
                    arguments: if call.arguments.trim().is_empty() {
                        "{}".into()
                    } else {
                        call.arguments
                    },
                })
            })
            .collect();
        out.push(ProviderEvent::Finished(
            self.finish.take().unwrap_or(FinishReason::Stop),
        ));
        out
    }

    pub fn is_done(&self) -> bool {
        self.done
    }
}

impl EventParser for ResponsesStreamParser {
    fn push(&mut self, data: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
        ResponsesStreamParser::push(self, data)
    }

    fn finish(&mut self) -> Vec<ProviderEvent> {
        ResponsesStreamParser::finish(self)
    }

    fn is_done(&self) -> bool {
        ResponsesStreamParser::is_done(self)
    }
}

/// The error an `error` event or a failed response reports. Server errors and rate limits are
/// worth retrying, as their HTTP forms are.
fn stream_error(error: &Value) -> ProviderError {
    let code = error["code"].as_str().unwrap_or_default();
    let message = error["message"].as_str().unwrap_or_default();
    let text = match (code, message) {
        ("", "") => error.to_string(),
        ("", message) => message.to_string(),
        (code, "") => code.to_string(),
        (code, message) => format!("{code}: {message}"),
    };
    let status = match code {
        "server_error" => 500,
        "rate_limit_exceeded" => 429,
        _ => return ProviderError::InStream(text),
    };
    ProviderError::Http {
        status,
        body: text,
        retry_after: None,
    }
}

/// A provider speaking the Responses protocol with an optional API key.
pub struct OpenAiResponses {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl OpenAiResponses {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        OpenAiResponses {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
        }
    }
}

impl Provider for OpenAiResponses {
    fn stream(&self, request: ChatRequest) -> ProviderStream {
        let mut http = self
            .client
            .post(format!("{}/responses", self.base_url))
            .json(&request_body(&request));
        if let Some(key) = &self.api_key {
            http = http.bearer_auth(key);
        }
        sse::events(sse::send(http), ResponsesStreamParser::default())
    }
}
```

In `crates/harness-providers/src/lib.rs`:

Replace:

```rust

pub mod discovery;
pub mod openai_chat;
pub mod registry;
```

with:

```rust

pub mod discovery;
pub mod openai_chat;
pub mod openai_responses;
pub mod registry;
mod sse;
```

In `crates/harness-providers/src/registry.rs`:

Replace (1 of 6):

```rust
use harness_config::config::{Protocol, ProviderConfig};
use harness_core::provider::Provider;

use crate::{discovery::Endpoint, openai_chat::OpenAiChat};

/// Providers usable without configuration: (name, base URL, API-key environment variable).
pub const BUILTIN_PROVIDERS: [(&str, &str, Option<&str>); 4] = [
    ("ollama", "http://127.0.0.1:11434/v1", None),
    ("lmstudio", "http://127.0.0.1:1234/v1", None),
    ("llamacpp", "http://127.0.0.1:8080/v1", None),
    (
        "openrouter",
        "https://openrouter.ai/api/v1",
        Some("OPENROUTER_API_KEY"),
    ),
];

const LOCAL_PROVIDERS: [&str; 3] = ["ollama", "lmstudio", "llamacpp"];
```

with:

```rust
use harness_config::config::{Protocol, ProviderConfig};
use harness_core::provider::Provider;

use crate::{discovery::Endpoint, openai_chat::OpenAiChat, openai_responses::OpenAiResponses};

/// A provider usable without configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Builtin {
    pub name: &'static str,
    pub protocol: Protocol,
    pub base_url: &'static str,
    /// The environment variable holding its API key, when it needs one.
    pub key_env: Option<&'static str>,
}

const fn builtin(
    name: &'static str,
    protocol: Protocol,
    base_url: &'static str,
    key_env: Option<&'static str>,
) -> Builtin {
    Builtin {
        name,
        protocol,
        base_url,
        key_env,
    }
}

/// Providers usable without configuration.
pub const BUILTIN_PROVIDERS: [Builtin; 5] = [
    builtin(
        "ollama",
        Protocol::OpenaiChat,
        "http://127.0.0.1:11434/v1",
        None,
    ),
    builtin(
        "lmstudio",
        Protocol::OpenaiChat,
        "http://127.0.0.1:1234/v1",
        None,
    ),
    builtin(
        "llamacpp",
        Protocol::OpenaiChat,
        "http://127.0.0.1:8080/v1",
        None,
    ),
    builtin(
        "openrouter",
        Protocol::OpenaiChat,
        "https://openrouter.ai/api/v1",
        Some("OPENROUTER_API_KEY"),
    ),
    builtin(
        "openai",
        Protocol::OpenaiResponses,
        "https://api.openai.com/v1",
        Some("OPENAI_API_KEY"),
    ),
];

const LOCAL_PROVIDERS: [&str; 3] = ["ollama", "lmstudio", "llamacpp"];
```

Replace (2 of 6):

```rust
    pub model: String,
    /// The full `<provider>/<model>` id.
    pub id: String,
}

pub fn resolve(
```

with:

```rust
    pub model: String,
    /// The full `<provider>/<model>` id.
    pub id: String,
    pub protocol: Protocol,
    pub base_url: String,
}

pub fn resolve(
```

Replace (3 of 6):

```rust
        .ok_or_else(|| ResolveError::BadId(model_id.to_string()))?;
    let (protocol, base_url, key_env) = if let Some(cfg) = providers.get(name) {
        (cfg.protocol, cfg.base_url.clone(), cfg.api_key_env.clone())
    } else if let Some((_, url, key)) = BUILTIN_PROVIDERS.iter().find(|(n, ..)| *n == name) {
        (Protocol::OpenaiChat, url.to_string(), key.map(String::from))
    } else {
        return Err(ResolveError::UnknownProvider(name.to_string()));
    };
```

with:

```rust
        .ok_or_else(|| ResolveError::BadId(model_id.to_string()))?;
    let (protocol, base_url, key_env) = if let Some(cfg) = providers.get(name) {
        (cfg.protocol, cfg.base_url.clone(), cfg.api_key_env.clone())
    } else if let Some(builtin) = BUILTIN_PROVIDERS.iter().find(|b| b.name == name) {
        (
            builtin.protocol,
            builtin.base_url.to_string(),
            builtin.key_env.map(String::from),
        )
    } else {
        return Err(ResolveError::UnknownProvider(name.to_string()));
    };
```

Replace (4 of 6):

```rust
            None => None,
        };
    let provider: Arc<dyn Provider> = match protocol {
        Protocol::OpenaiChat => Arc::new(OpenAiChat::new(base_url, api_key)),
    };
    Ok(Resolved {
        provider,
        model: model.to_string(),
        id: model_id.to_string(),
    })
}

```

with:

```rust
            None => None,
        };
    let provider: Arc<dyn Provider> = match protocol {
        Protocol::OpenaiChat => Arc::new(OpenAiChat::new(base_url.clone(), api_key)),
        Protocol::OpenaiResponses => Arc::new(OpenAiResponses::new(base_url.clone(), api_key)),
    };
    Ok(Resolved {
        provider,
        model: model.to_string(),
        id: model_id.to_string(),
        protocol,
        base_url,
    })
}

```

Replace (5 of 6):

```rust
pub fn local_endpoints(providers: &BTreeMap<String, ProviderConfig>) -> Vec<Endpoint> {
    BUILTIN_PROVIDERS
        .iter()
        .filter(|(name, ..)| LOCAL_PROVIDERS.contains(name) && !providers.contains_key(*name))
        .map(|(name, url, _)| Endpoint {
            provider: name.to_string(),
            base_url: url.to_string(),
            api_key: None,
        })
        .collect()
}

/// Configured providers whose API key (if one is required) is present, plus any built-in
/// provider that needs a key (currently just openrouter) whose key is set and that the user
/// hasn't redefined under `[providers.<name>]`.
pub fn configured_endpoints(
    providers: &BTreeMap<String, ProviderConfig>,
    env: impl Fn(&str) -> Option<String>,
```

with:

```rust
pub fn local_endpoints(providers: &BTreeMap<String, ProviderConfig>) -> Vec<Endpoint> {
    BUILTIN_PROVIDERS
        .iter()
        .filter(|b| LOCAL_PROVIDERS.contains(&b.name) && !providers.contains_key(b.name))
        .map(|b| Endpoint {
            provider: b.name.to_string(),
            base_url: b.base_url.to_string(),
            api_key: None,
        })
        .collect()
}

/// Configured providers whose API key (if one is required) is present, plus any built-in
/// provider that needs a key (openrouter, openai) whose key is set and that the user hasn't
/// redefined under `[providers.<name>]`.
pub fn configured_endpoints(
    providers: &BTreeMap<String, ProviderConfig>,
    env: impl Fn(&str) -> Option<String>,
```

Replace (6 of 6):

```rust
        })
        .collect();

    for (name, url, key_env) in BUILTIN_PROVIDERS {
        if LOCAL_PROVIDERS.contains(&name) || providers.contains_key(name) {
            continue;
        }
        if let Some(api_key) = key_env.and_then(&env).filter(|v| !v.is_empty()) {
            endpoints.push(Endpoint {
                provider: name.to_string(),
                base_url: url.to_string(),
                api_key: Some(api_key),
            });
        }
```

with:

```rust
        })
        .collect();

    for builtin in BUILTIN_PROVIDERS {
        if LOCAL_PROVIDERS.contains(&builtin.name) || providers.contains_key(builtin.name) {
            continue;
        }
        if let Some(api_key) = builtin.key_env.and_then(&env).filter(|v| !v.is_empty()) {
            endpoints.push(Endpoint {
                provider: builtin.name.to_string(),
                base_url: builtin.base_url.to_string(),
                api_key: Some(api_key),
            });
        }
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p harness-providers -p harness-config -p harness-core`
Expected: PASS: 11 in `openai_responses_parser`, 5 in `openai_responses_http`, 10 in `registry`, 33 in `config` and 23 in `agent` (among them `the_configured_request_options_reach_the_provider`); 340 tests in the three crates.

- [ ] **Step 7: Lint**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add crates/harness-core crates/harness-config crates/harness-providers
git commit -F - <<'EOF'
feat(providers): add the OpenAI Responses adapter

The openai-responses protocol streams replies from POST /responses:
text, reasoning summaries, function calls emitted whole, usage, and a
Length finish when the output limit cut the reply. Requests are
stateless (store: false) and carry the profile's output limit,
temperature and reasoning effort, which the agent now passes in
ChatRequest::options. The built-in openai provider uses it with
OPENAI_API_KEY. The adapters share one server-sent-events reader.

<trailer lines from the controller>
EOF
```

---

### Task 3: The Anthropic Messages adapter

**Files:**
- Create: `crates/harness-providers/src/anthropic_messages.rs`, `crates/harness-providers/tests/anthropic_messages_parser.rs`, `crates/harness-providers/tests/anthropic_messages_http.rs`, and in `crates/harness-providers/tests/fixtures/anthropic-messages/`: `text.sse`, `tool_use.sse`, `max_tokens.sse`, `overloaded.sse`
- Modify: `crates/harness-config/src/config.rs`, `crates/harness-config/tests/config.rs`, `crates/harness-providers/src/lib.rs`, `crates/harness-providers/src/discovery.rs`, `crates/harness-providers/src/registry.rs`, `crates/harness-providers/tests/discovery.rs`, `crates/harness-providers/tests/registry.rs`

**Interfaces:**
- Consumes: `sse::{EventParser, events, send}`, `Resolved::{protocol, base_url}`, `Builtin` (Task 2).
- Produces:
  - `harness_config::config::Protocol::AnthropicMessages`, written `"anthropic-messages"`.
  - `harness_providers::anthropic_messages::{API_VERSION = "2023-06-01", DEFAULT_MAX_TOKENS = 16_384, request_body(req: &ChatRequest) -> Value, MessagesStreamParser, AnthropicMessages::new(base_url, api_key: Option<String>)}`, the parser with the same three methods.
  - `discovery::Endpoint` gains `protocol: Protocol`: Anthropic's `/models` is asked with `x-api-key` and `anthropic-version`.
  - `BUILTIN_PROVIDERS: [Builtin; 6]`, now with `anthropic` (`https://api.anthropic.com/v1`, `ANTHROPIC_API_KEY`).

Anthropic's input tokens exclude what the prompt cache wrote and read, so the usage harness reports adds `cache_creation_input_tokens` and `cache_read_input_tokens` to `input_tokens`: compaction measures the whole request. P3's review F asked to check that Anthropic's context overflows are recognised: P3 already matches "prompt is too long" and "exceed context limit", and this task tests both through the adapter.

- [ ] **Step 1: Write the failing tests**

The fixtures follow Anthropic's documented streaming examples.

Create `crates/harness-providers/tests/fixtures/anthropic-messages/text.sse`:

```text
event: message_start
data: {"type": "message_start", "message": {"id": "msg_01", "type": "message", "role": "assistant", "content": [], "model": "claude-sonnet-4-5", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 25, "cache_creation_input_tokens": 100, "cache_read_input_tokens": 2000, "output_tokens": 1}}}

event: content_block_start
data: {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}

event: ping
data: {"type": "ping"}

event: content_block_delta
data: {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hello"}}

event: content_block_delta
data: {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "!"}}

event: content_block_stop
data: {"type": "content_block_stop", "index": 0}

event: message_delta
data: {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null}, "usage": {"output_tokens": 15}}

event: message_stop
data: {"type": "message_stop"}

```

Create `crates/harness-providers/tests/fixtures/anthropic-messages/tool_use.sse`:

```text
event: message_start
data: {"type":"message_start","message":{"id":"msg_02","type":"message","role":"assistant","model":"claude-sonnet-4-5","stop_sequence":null,"usage":{"input_tokens":472,"output_tokens":2},"content":[],"stop_reason":null}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Let me read it."}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01T1x1fJ34qAmk2tNTrN7Up6","name":"read","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\": \"src/"}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"lib.rs\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":89}}

event: message_stop
data: {"type":"message_stop"}

```

Create `crates/harness-providers/tests/fixtures/anthropic-messages/max_tokens.sse`:

```text
event: message_start
data: {"type":"message_start","message":{"id":"msg_03","type":"message","role":"assistant","model":"claude-sonnet-4-5","stop_sequence":null,"usage":{"input_tokens":30,"output_tokens":1},"content":[],"stop_reason":null}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"The file is large."}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EqQBCgIYAhIM"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_cut","name":"write","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\": \"big.txt\", \"content\": \"aaaa"}}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"max_tokens","stop_sequence":null},"usage":{"output_tokens":16384}}

event: message_stop
data: {"type":"message_stop"}

```

Create `crates/harness-providers/tests/fixtures/anthropic-messages/overloaded.sse`:

```text
event: message_start
data: {"type":"message_start","message":{"id":"msg_04","type":"message","role":"assistant","model":"claude-sonnet-4-5","stop_sequence":null,"usage":{"input_tokens":30,"output_tokens":1},"content":[],"stop_reason":null}}

event: error
data: {"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}

```

Create `crates/harness-providers/tests/anthropic_messages_parser.rs`:

```rust
//! The Messages stream parser and request body, against fixtures built from the documented
//! streaming examples (`tests/fixtures/anthropic-messages/`).

use harness_core::message::{ChatRequest, Message, RequestOptions, ToolCall, ToolSpec, Usage};
use harness_core::provider::{FinishReason, ProviderError, ProviderEvent};
use harness_providers::anthropic_messages::{
    DEFAULT_MAX_TOKENS, MessagesStreamParser, request_body,
};
use serde_json::json;

fn payloads(name: &str) -> Vec<String> {
    let path = format!(
        "{}/tests/fixtures/anthropic-messages/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("data: ").map(String::from))
        .collect()
}

fn parse(name: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
    let mut parser = MessagesStreamParser::default();
    let mut events = Vec::new();
    for data in payloads(name) {
        events.extend(parser.push(&data)?);
        if parser.is_done() {
            break;
        }
    }
    events.extend(parser.finish());
    Ok(events)
}

#[test]
fn text_arrives_as_deltas_and_usage_counts_cached_input() {
    assert_eq!(
        parse("text.sse").unwrap(),
        vec![
            ProviderEvent::TextDelta("Hello".into()),
            ProviderEvent::TextDelta("!".into()),
            // Input is what was sent: uncached, written to the cache, and read from it.
            ProviderEvent::Usage(Usage {
                input_tokens: 2125,
                output_tokens: 15,
                cached_tokens: 2000,
            }),
            ProviderEvent::Finished(FinishReason::Stop),
        ]
    );
}

#[test]
fn tool_use_is_emitted_whole_after_the_text() {
    let events = parse("tool_use.sse").unwrap();
    assert_eq!(
        events[0],
        ProviderEvent::TextDelta("Let me read it.".into())
    );
    assert_eq!(
        events[2..],
        [
            ProviderEvent::ToolCall(ToolCall {
                id: "toolu_01T1x1fJ34qAmk2tNTrN7Up6".into(),
                name: "read".into(),
                arguments: r#"{"path": "src/lib.rs"}"#.into(),
            }),
            ProviderEvent::Finished(FinishReason::ToolCalls),
        ]
    );
    assert!(matches!(events[1], ProviderEvent::Usage(_)));
}

#[test]
fn a_reply_stopped_by_max_tokens_finishes_with_length() {
    let events = parse("max_tokens.sse").unwrap();
    assert_eq!(
        events[0],
        ProviderEvent::ReasoningDelta("The file is large.".into())
    );
    assert!(events.contains(&ProviderEvent::ToolCall(ToolCall {
        id: "toolu_cut".into(),
        name: "write".into(),
        arguments: r#"{"path": "big.txt", "content": "aaaa"#.into(),
    })));
    assert_eq!(
        events.last(),
        Some(&ProviderEvent::Finished(FinishReason::Length))
    );
}

#[test]
fn an_overloaded_error_in_the_stream_can_be_retried() {
    let error = parse("overloaded.sse").unwrap_err();
    assert!(
        matches!(error, ProviderError::Http { status: 529, .. }),
        "{error:?}"
    );
    assert!(error.is_retryable());
}

#[test]
fn other_stream_errors_name_their_type() {
    let mut parser = MessagesStreamParser::default();
    let data = json!({"type": "error", "error": {"type": "invalid_request_error",
        "message": "prompt is too long: 208000 tokens > 200000 maximum"}});
    let error = parser.push(&data.to_string()).unwrap_err();
    assert_eq!(
        error,
        ProviderError::InStream(
            "invalid_request_error: prompt is too long: 208000 tokens > 200000 maximum".into()
        )
    );
    assert!(error.is_context_overflow());
}

#[test]
fn finish_is_idempotent() {
    let mut parser = MessagesStreamParser::default();
    for data in payloads("text.sse") {
        parser.push(&data).unwrap();
    }
    assert!(parser.is_done());
    assert!(parser.finish().is_empty());
}

fn call(id: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "read".into(),
        arguments: arguments.into(),
    }
}

fn request(messages: Vec<Message>) -> ChatRequest {
    ChatRequest {
        model: "claude-sonnet-4-5".into(),
        system: "be brief".into(),
        messages,
        tools: vec![ToolSpec {
            name: "read".into(),
            description: "Read".into(),
            parameters: json!({"type": "object"}),
        }],
        options: RequestOptions::default(),
    }
}

#[test]
fn the_request_uses_the_messages_shapes_and_marks_cache_breakpoints() {
    let body = request_body(&request(vec![Message::User {
        content: "hi".into(),
    }]));
    assert_eq!(body["model"], "claude-sonnet-4-5");
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
    assert_eq!(
        body["system"],
        json!([{"type": "text", "text": "be brief", "cache_control": {"type": "ephemeral"}}])
    );
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": [
            {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}
        ]}])
    );
    assert_eq!(
        body["tools"],
        json!([{"name": "read", "description": "Read", "input_schema": {"type": "object"}}])
    );
    assert!(body.get("temperature").is_none());
    assert!(body.get("thinking").is_none());
}

#[test]
fn tool_results_and_the_next_prompt_share_one_user_message() {
    let body = request_body(&request(vec![
        Message::User {
            content: "read a and b".into(),
        },
        Message::Assistant {
            content: String::new(),
            tool_calls: vec![call("t1", r#"{"path":"a"}"#), call("t2", "{not json")],
            model: "anthropic/claude-sonnet-4-5".into(),
        },
        Message::Tool {
            call_id: "t1".into(),
            content: "A".into(),
            is_error: false,
        },
        Message::Tool {
            call_id: "t2".into(),
            content: "arguments for `read` are not valid JSON".into(),
            is_error: true,
        },
        Message::User {
            content: "[harness] The approval mode is now ask".into(),
        },
    ]));
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3, "{messages:?}");
    assert_eq!(
        messages[1],
        json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "t1", "name": "read", "input": {"path": "a"}},
            // Anthropic takes only objects: an invalid call is sent with no arguments.
            {"type": "tool_use", "id": "t2", "name": "read", "input": {}},
        ]})
    );
    assert_eq!(
        messages[2],
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "A"},
            {"type": "tool_result", "tool_use_id": "t2",
                "content": "arguments for `read` are not valid JSON", "is_error": true},
            {"type": "text", "text": "[harness] The approval mode is now ask",
                "cache_control": {"type": "ephemeral"}},
        ]})
    );
}

// The API rejects empty text blocks and messages without content.
#[test]
fn empty_assistant_messages_and_empty_text_are_left_out() {
    let body = request_body(&request(vec![
        Message::User {
            content: "one".into(),
        },
        Message::Assistant {
            content: String::new(),
            tool_calls: vec![],
            model: "anthropic/claude-sonnet-4-5".into(),
        },
        Message::User {
            content: "two".into(),
        },
    ]));
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": [
            {"type": "text", "text": "one"},
            {"type": "text", "text": "two", "cache_control": {"type": "ephemeral"}},
        ]}])
    );
    let mut no_system = request(vec![Message::User {
        content: "hi".into(),
    }]);
    no_system.system.clear();
    assert!(request_body(&no_system).get("system").is_none());
}

#[test]
fn profile_options_reach_the_request() {
    let mut req = request(vec![Message::User {
        content: "hi".into(),
    }]);
    req.options = RequestOptions {
        max_output_tokens: Some(4096),
        temperature: Some(0.2),
        reasoning_effort: Some("high".into()),
    };
    let body = request_body(&req);
    assert_eq!(body["max_tokens"], 4096);
    assert_eq!(body["temperature"], 0.2);
    // Extended thinking is not requested in M1.
    assert!(body.get("thinking").is_none());
}
```

Create `crates/harness-providers/tests/anthropic_messages_http.rs`:

```rust
//! The Messages provider over HTTP, against a mock server replaying the fixtures.

use futures::StreamExt;
use harness_core::message::{ChatRequest, Message};
use harness_core::provider::{FinishReason, Provider, ProviderError, ProviderEvent};
use harness_providers::anthropic_messages::AnthropicMessages;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/anthropic-messages/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn sse(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "claude-sonnet-4-5".into(),
        system: "s".into(),
        messages: vec![Message::User {
            content: "hi".into(),
        }],
        ..ChatRequest::default()
    }
}

#[tokio::test]
async fn streams_a_reply_with_the_api_key_and_version() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-ant-api03-test"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(sse(fixture("text.sse")))
        .mount(&server)
        .await;
    let provider = AnthropicMessages::new(
        format!("{}/v1", server.uri()),
        Some("sk-ant-api03-test".into()),
    );
    let events: Vec<_> = provider.stream(request()).collect().await;
    assert_eq!(events[0], Ok(ProviderEvent::TextDelta("Hello".into())));
    assert_eq!(
        events.last(),
        Some(&Ok(ProviderEvent::Finished(FinishReason::Stop)))
    );
    // The key goes in its own header, never as a bearer token.
    let sent = &server.received_requests().await.unwrap()[0];
    assert!(sent.headers.get("authorization").is_none());
}

// Carried from P3's review: both of Anthropic's overflow errors must lead to compaction.
#[tokio::test]
async fn both_context_overflow_errors_are_recognised() {
    for message in [
        "prompt is too long: 208310 tokens > 200000 maximum",
        "input length and `max_tokens` exceed context limit: 188240 + 21333 > 200000, decrease input length or `max_tokens` and try again",
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_string(format!(
                r#"{{"type":"error","error":{{"type":"invalid_request_error","message":"{}"}}}}"#,
                message
            )))
            .mount(&server)
            .await;
        let provider = AnthropicMessages::new(format!("{}/v1", server.uri()), Some("k".into()));
        let error = provider
            .stream(request())
            .next()
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.is_context_overflow(), "{error}");
    }
}

#[tokio::test]
async fn an_overloaded_stream_is_retryable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(fixture("overloaded.sse")))
        .mount(&server)
        .await;
    let provider = AnthropicMessages::new(format!("{}/v1", server.uri()), Some("k".into()));
    let events: Vec<_> = provider.stream(request()).collect().await;
    assert!(
        matches!(events.last(), Some(Err(e)) if e.is_retryable()),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_stream_cut_before_message_stop_is_a_network_error() {
    let server = MockServer::start().await;
    let cut = fixture("text.sse")
        .split("event: message_delta")
        .next()
        .unwrap()
        .to_string();
    Mock::given(method("POST"))
        .respond_with(sse(cut))
        .mount(&server)
        .await;
    let provider = AnthropicMessages::new(format!("{}/v1", server.uri()), Some("k".into()));
    let events: Vec<_> = provider.stream(request()).collect().await;
    assert!(
        matches!(events.last(), Some(Err(ProviderError::Network(_)))),
        "{events:?}"
    );
}
```

In `crates/harness-providers/tests/discovery.rs`, the helper sets the new field, and a test covers Anthropic's listing:

Replace (1 of 3):

```rust
use std::time::{Duration, Instant};

use harness_providers::discovery::{DiscoveredModel, Endpoint, list_models};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn endpoint(provider: &str, base_url: String) -> Endpoint {
```

with:

```rust
use std::time::{Duration, Instant};

use harness_config::config::Protocol;
use harness_providers::discovery::{DiscoveredModel, Endpoint, list_models};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn endpoint(provider: &str, base_url: String) -> Endpoint {
```

Replace (2 of 3):

```rust
        provider: provider.into(),
        base_url,
        api_key: None,
    }
}

```

with:

```rust
        provider: provider.into(),
        base_url,
        api_key: None,
        protocol: Protocol::OpenaiChat,
    }
}

```

Replace (3 of 3):

```rust
    .await;
    assert!(found.is_empty());
}
```

with:

```rust
    .await;
    assert!(found.is_empty());
}

#[tokio::test]
async fn anthropic_endpoints_are_listed_with_their_own_headers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("x-api-key", "sk-ant-api03-k"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"type": "model", "id": "claude-sonnet-4-5", "display_name": "Claude Sonnet 4.5"}],
            "has_more": false
        })))
        .mount(&server)
        .await;
    let found = list_models(
        &[Endpoint {
            provider: "anthropic".into(),
            base_url: format!("{}/v1", server.uri()),
            api_key: Some("sk-ant-api03-k".into()),
            protocol: Protocol::AnthropicMessages,
        }],
        Duration::from_secs(3),
    )
    .await;
    assert_eq!(
        found,
        vec![DiscoveredModel {
            provider: "anthropic".into(),
            name: "claude-sonnet-4-5".into()
        }]
    );
}
```

Append to `crates/harness-providers/tests/registry.rs`, after a blank line:

```rust
#[test]
fn builtin_anthropic_speaks_the_messages_protocol_with_its_key() {
    let r = resolve(
        "anthropic/claude-sonnet-4-5",
        &BTreeMap::new(),
        env(&[("ANTHROPIC_API_KEY", "sk-ant-api03-k")]),
    )
    .unwrap();
    assert_eq!(r.model, "claude-sonnet-4-5");
    assert_eq!(r.protocol, Protocol::AnthropicMessages);
    assert_eq!(r.base_url, "https://api.anthropic.com/v1");
    assert_eq!(
        resolve("anthropic/claude-sonnet-4-5", &BTreeMap::new(), env(&[])).err(),
        Some(ResolveError::MissingKey {
            provider: "anthropic".into(),
            var: "ANTHROPIC_API_KEY".into()
        })
    );
    let found = configured_endpoints(
        &BTreeMap::new(),
        env(&[("ANTHROPIC_API_KEY", "sk-ant-api03-k")]),
    );
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].protocol, Protocol::AnthropicMessages);
}
```

Append to `crates/harness-config/tests/config.rs`, after a blank line:

```rust
#[test]
fn providers_may_use_the_messages_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    std::fs::write(
        &file,
        "[providers.local-claude]\nprotocol = \"anthropic-messages\"\nbase_url = \"http://127.0.0.1:4000/v1\"\n",
    )
    .unwrap();
    let parsed = config::parse_file(&file).unwrap().unwrap();
    assert_eq!(
        parsed.providers["local-claude"].protocol,
        config::Protocol::AnthropicMessages
    );
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p harness-providers --test anthropic_messages_parser; cargo test -p harness-providers --test anthropic_messages_http; cargo test -p harness-providers --test discovery; cargo test -p harness-providers --test registry; cargo test -p harness-config --test config`
Expected: FAIL to compile. `anthropic_messages_parser` and `anthropic_messages_http`: ``unresolved import `harness_providers::anthropic_messages` ``. `discovery`: ``struct `Endpoint` has no field named `protocol` `` and ``no variant … named `AnthropicMessages` found for enum `Protocol` ``. `registry`: the same missing variant, and ``no field `protocol` on type `Endpoint` ``. `config`: ``no variant … named `AnthropicMessages` found for enum `harness_config::config::Protocol` ``.

- [ ] **Step 3: Add the protocol**

In `crates/harness-config/src/config.rs`:

Replace:

```rust
    OpenaiChat,
    /// `POST /responses`: OpenAI API keys and ChatGPT sign-in.
    OpenaiResponses,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
```

with:

```rust
    OpenaiChat,
    /// `POST /responses`: OpenAI API keys and ChatGPT sign-in.
    OpenaiResponses,
    /// `POST /messages`: Anthropic API keys and Anthropic-compatible servers.
    AnthropicMessages,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
```

- [ ] **Step 4: Add the adapter**

Create `crates/harness-providers/src/anthropic_messages.rs`:

```rust
//! The Anthropic Messages protocol (`POST /messages`, server-sent events), with an API key only:
//! Claude subscription credentials are never used.

use std::collections::BTreeMap;

use harness_core::{
    message::{ChatRequest, Message, ToolCall, Usage},
    provider::{FinishReason, Provider, ProviderError, ProviderEvent, ProviderStream},
};
use serde_json::{Map, Value, json};

use crate::sse::{self, EventParser};

/// The API version harness speaks.
pub const API_VERSION: &str = "2023-06-01";
/// `max_tokens` when the profile sets no output limit: the protocol requires one.
pub const DEFAULT_MAX_TOKENS: u64 = 16_384;

/// Builds a streaming Messages request body. Consecutive messages of one role become one message:
/// tool results are user content here, so a note or prompt after them joins their message.
/// Empty text and empty assistant messages are left out, since the API rejects them. The system
/// prompt and the last message are prompt-cache breakpoints.
pub fn request_body(req: &ChatRequest) -> Value {
    let mut messages: Vec<(&str, Vec<Value>)> = Vec::new();
    let mut push = |role: &'static str, blocks: Vec<Value>| {
        if blocks.is_empty() {
            return;
        }
        match messages.last_mut() {
            Some((last, content)) if *last == role => content.extend(blocks),
            _ => messages.push((role, blocks)),
        }
    };
    for message in &req.messages {
        match message {
            Message::User { content } => push("user", text_block(content)),
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => {
                let mut blocks = text_block(content);
                for call in tool_calls {
                    // Only an object is accepted: a call whose arguments were not valid JSON is
                    // sent without them (its result says what was wrong).
                    let input = serde_json::from_str::<Value>(&call.arguments)
                        .ok()
                        .filter(Value::is_object)
                        .unwrap_or_else(|| Value::Object(Map::new()));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": input,
                    }));
                }
                push("assistant", blocks);
            }
            Message::Tool {
                call_id,
                content,
                is_error,
            } => {
                let mut block = json!({"type": "tool_result", "tool_use_id": call_id});
                if !content.is_empty() {
                    block["content"] = json!(content);
                }
                if *is_error {
                    block["is_error"] = json!(true);
                }
                push("user", vec![block]);
            }
        }
    }
    if let Some(block) = messages
        .last_mut()
        .and_then(|(_, content)| content.last_mut())
    {
        block["cache_control"] = json!({"type": "ephemeral"});
    }
    let messages: Vec<Value> = messages
        .into_iter()
        .map(|(role, content)| json!({"role": role, "content": content}))
        .collect();
    let mut body = json!({
        "model": req.model,
        "max_tokens": req.options.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "messages": messages,
        "stream": true,
    });
    if !req.system.is_empty() {
        body["system"] = json!([{
            "type": "text",
            "text": req.system,
            "cache_control": {"type": "ephemeral"},
        }]);
    }
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .map(|t| {
                    json!({"name": t.name, "description": t.description, "input_schema": t.parameters})
                })
                .collect(),
        );
    }
    if let Some(temperature) = req.options.temperature {
        body["temperature"] = json!(temperature);
    }
    body
}

/// A text block, or none for empty text.
fn text_block(text: &str) -> Vec<Value> {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![json!({"type": "text", "text": text})]
    }
}

#[derive(Debug, Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

/// Turns Messages stream events into [`ProviderEvent`]s. Tool calls are buffered by content block
/// and emitted whole, with the usage, when the message stops.
#[derive(Debug, Default)]
pub struct MessagesStreamParser {
    calls: BTreeMap<u64, PartialCall>,
    usage: Option<Usage>,
    finish: Option<FinishReason>,
    done: bool,
}

impl MessagesStreamParser {
    pub fn push(&mut self, data: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
        let event: Value = serde_json::from_str(data)
            .map_err(|e| ProviderError::Protocol(format!("{e} in event: {data}")))?;
        let mut out = Vec::new();
        match event["type"].as_str().unwrap_or_default() {
            "message_start" => {
                let usage = &event["message"]["usage"];
                let count = |key: &str| usage[key].as_u64().unwrap_or(0);
                // Input is everything sent: uncached, written to the cache, and read from it.
                self.usage = Some(Usage {
                    input_tokens: count("input_tokens")
                        + count("cache_creation_input_tokens")
                        + count("cache_read_input_tokens"),
                    output_tokens: count("output_tokens"),
                    cached_tokens: count("cache_read_input_tokens"),
                });
            }
            "content_block_start" => {
                let block = &event["content_block"];
                if block["type"] == "tool_use" {
                    let index = event["index"].as_u64().unwrap_or(0);
                    let call = self.calls.entry(index).or_default();
                    call.id = block["id"].as_str().unwrap_or_default().to_string();
                    call.name = block["name"].as_str().unwrap_or_default().to_string();
                }
            }
            "content_block_delta" => {
                let delta = &event["delta"];
                match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" => {
                        if let Some(text) = delta["text"].as_str().filter(|t| !t.is_empty()) {
                            out.push(ProviderEvent::TextDelta(text.to_string()));
                        }
                    }
                    "thinking_delta" => {
                        if let Some(text) = delta["thinking"].as_str().filter(|t| !t.is_empty()) {
                            out.push(ProviderEvent::ReasoningDelta(text.to_string()));
                        }
                    }
                    "input_json_delta" => {
                        let index = event["index"].as_u64().unwrap_or(0);
                        if let Some(fragment) = delta["partial_json"].as_str() {
                            self.calls
                                .entry(index)
                                .or_default()
                                .arguments
                                .push_str(fragment);
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(reason) = event["delta"]["stop_reason"].as_str() {
                    self.finish = Some(match reason {
                        "end_turn" | "stop_sequence" => FinishReason::Stop,
                        "tool_use" => FinishReason::ToolCalls,
                        "max_tokens" => FinishReason::Length,
                        other => FinishReason::Other(other.to_string()),
                    });
                }
                if let (Some(usage), Some(output)) = (
                    self.usage.as_mut(),
                    event["usage"]["output_tokens"].as_u64(),
                ) {
                    usage.output_tokens = output;
                }
            }
            "message_stop" => out.extend(self.finish()),
            "error" => return Err(stream_error(&event["error"])),
            _ => {}
        }
        Ok(out)
    }

    /// Emits the usage, buffered calls, and `Finished`. Calling it again yields nothing.
    pub fn finish(&mut self) -> Vec<ProviderEvent> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        let mut out: Vec<ProviderEvent> = self
            .usage
            .take()
            .map(ProviderEvent::Usage)
            .into_iter()
            .collect();
        out.extend(std::mem::take(&mut self.calls).into_values().map(|call| {
            ProviderEvent::ToolCall(ToolCall {
                id: call.id,
                name: call.name,
                arguments: if call.arguments.trim().is_empty() {
                    "{}".into()
                } else {
                    call.arguments
                },
            })
        }));
        out.push(ProviderEvent::Finished(
            self.finish.take().unwrap_or(FinishReason::Stop),
        ));
        out
    }

    pub fn is_done(&self) -> bool {
        self.done
    }
}

impl EventParser for MessagesStreamParser {
    fn push(&mut self, data: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
        MessagesStreamParser::push(self, data)
    }

    fn finish(&mut self) -> Vec<ProviderEvent> {
        MessagesStreamParser::finish(self)
    }

    fn is_done(&self) -> bool {
        MessagesStreamParser::is_done(self)
    }
}

/// The error an `error` event reports. Overload, API and rate-limit errors are retried as their
/// HTTP forms (529, 500, 429) are.
fn stream_error(error: &Value) -> ProviderError {
    let kind = error["type"].as_str().unwrap_or_default();
    let message = error["message"].as_str().unwrap_or_default();
    let text = if kind.is_empty() {
        error.to_string()
    } else {
        format!("{kind}: {message}")
    };
    let status = match kind {
        "overloaded_error" => 529,
        "api_error" => 500,
        "rate_limit_error" => 429,
        _ => return ProviderError::InStream(text),
    };
    ProviderError::Http {
        status,
        body: text,
        retry_after: None,
    }
}

/// A provider speaking the Messages protocol with an Anthropic API key.
pub struct AnthropicMessages {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl AnthropicMessages {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        AnthropicMessages {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
        }
    }
}

impl Provider for AnthropicMessages {
    fn stream(&self, request: ChatRequest) -> ProviderStream {
        let mut http = self
            .client
            .post(format!("{}/messages", self.base_url))
            .header("anthropic-version", API_VERSION)
            .json(&request_body(&request));
        if let Some(key) = &self.api_key {
            http = http.header("x-api-key", key);
        }
        sse::events(sse::send(http), MessagesStreamParser::default())
    }
}
```

In `crates/harness-providers/src/lib.rs`:

Replace:

```rust
//! Model provider adapters, local-server discovery, and model-id resolution.

pub mod discovery;
pub mod openai_chat;
pub mod openai_responses;
```

with:

```rust
//! Model provider adapters, local-server discovery, and model-id resolution.

pub mod anthropic_messages;
pub mod discovery;
pub mod openai_chat;
pub mod openai_responses;
```

- [ ] **Step 5: Add the `anthropic` provider and its model listing**

In `crates/harness-providers/src/discovery.rs`:

Replace (1 of 3):

```rust
use std::time::Duration;

use serde_json::Value;

/// Each local-server probe gives up after this long, so absent servers never delay startup.
pub const LOCAL_PROBE_TIMEOUT: Duration = Duration::from_millis(300);
/// Remote `/models` listings get longer.
```

with:

```rust
use std::time::Duration;

use harness_config::config::Protocol;
use serde_json::Value;

use crate::anthropic_messages::API_VERSION;

/// Each local-server probe gives up after this long, so absent servers never delay startup.
pub const LOCAL_PROBE_TIMEOUT: Duration = Duration::from_millis(300);
/// Remote `/models` listings get longer.
```

Replace (2 of 3):

```rust
    pub provider: String,
    pub base_url: String,
    pub api_key: Option<String>,
}

/// Lists models from OpenAI-compatible `/models` endpoints concurrently. Unreachable, slow, or failing
/// endpoints are skipped. Results keep endpoint order; models within an endpoint are sorted by name.
pub async fn list_models(endpoints: &[Endpoint], timeout: Duration) -> Vec<DiscoveredModel> {
    let Ok(client) = reqwest::Client::builder().timeout(timeout).build() else {
```

with:

```rust
    pub provider: String,
    pub base_url: String,
    pub api_key: Option<String>,
    /// How the key is sent: Anthropic's own headers, or a bearer token.
    pub protocol: Protocol,
}

/// Lists models from `/models` endpoints (OpenAI-compatible, or Anthropic's, which answers in the
/// same shape) concurrently. Unreachable, slow, or failing
/// endpoints are skipped. Results keep endpoint order; models within an endpoint are sorted by name.
pub async fn list_models(endpoints: &[Endpoint], timeout: Duration) -> Vec<DiscoveredModel> {
    let Ok(client) = reqwest::Client::builder().timeout(timeout).build() else {
```

Replace (3 of 3):

```rust
                "{}/models",
                endpoint.base_url.trim_end_matches('/')
            ));
            if let Some(key) = &endpoint.api_key {
                request = request.bearer_auth(key);
            }
            let Ok(response) = request.send().await.and_then(|r| r.error_for_status()) else {
```

with:

```rust
                "{}/models",
                endpoint.base_url.trim_end_matches('/')
            ));
            if endpoint.protocol == Protocol::AnthropicMessages {
                request = request.header("anthropic-version", API_VERSION);
                if let Some(key) = &endpoint.api_key {
                    request = request.header("x-api-key", key);
                }
            } else if let Some(key) = &endpoint.api_key {
                request = request.bearer_auth(key);
            }
            let Ok(response) = request.send().await.and_then(|r| r.error_for_status()) else {
```

In `crates/harness-providers/src/registry.rs`:

Replace (1 of 7):

```rust
use harness_config::config::{Protocol, ProviderConfig};
use harness_core::provider::Provider;

use crate::{discovery::Endpoint, openai_chat::OpenAiChat, openai_responses::OpenAiResponses};

/// A provider usable without configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
```

with:

```rust
use harness_config::config::{Protocol, ProviderConfig};
use harness_core::provider::Provider;

use crate::{
    anthropic_messages::AnthropicMessages, discovery::Endpoint, openai_chat::OpenAiChat,
    openai_responses::OpenAiResponses,
};

/// A provider usable without configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
```

Replace (2 of 7):

```rust
}

/// Providers usable without configuration.
pub const BUILTIN_PROVIDERS: [Builtin; 5] = [
    builtin(
        "ollama",
        Protocol::OpenaiChat,
```

with:

```rust
}

/// Providers usable without configuration.
pub const BUILTIN_PROVIDERS: [Builtin; 6] = [
    builtin(
        "ollama",
        Protocol::OpenaiChat,
```

Replace (3 of 7):

```rust
        "https://api.openai.com/v1",
        Some("OPENAI_API_KEY"),
    ),
];

const LOCAL_PROVIDERS: [&str; 3] = ["ollama", "lmstudio", "llamacpp"];
```

with:

```rust
        "https://api.openai.com/v1",
        Some("OPENAI_API_KEY"),
    ),
    builtin(
        "anthropic",
        Protocol::AnthropicMessages,
        "https://api.anthropic.com/v1",
        Some("ANTHROPIC_API_KEY"),
    ),
];

const LOCAL_PROVIDERS: [&str; 3] = ["ollama", "lmstudio", "llamacpp"];
```

Replace (4 of 7):

```rust
    let provider: Arc<dyn Provider> = match protocol {
        Protocol::OpenaiChat => Arc::new(OpenAiChat::new(base_url.clone(), api_key)),
        Protocol::OpenaiResponses => Arc::new(OpenAiResponses::new(base_url.clone(), api_key)),
    };
    Ok(Resolved {
        provider,
```

with:

```rust
    let provider: Arc<dyn Provider> = match protocol {
        Protocol::OpenaiChat => Arc::new(OpenAiChat::new(base_url.clone(), api_key)),
        Protocol::OpenaiResponses => Arc::new(OpenAiResponses::new(base_url.clone(), api_key)),
        Protocol::AnthropicMessages => Arc::new(AnthropicMessages::new(base_url.clone(), api_key)),
    };
    Ok(Resolved {
        provider,
```

Replace (5 of 7):

```rust
            provider: b.name.to_string(),
            base_url: b.base_url.to_string(),
            api_key: None,
        })
        .collect()
}

/// Configured providers whose API key (if one is required) is present, plus any built-in
/// provider that needs a key (openrouter, openai) whose key is set and that the user hasn't
/// redefined under `[providers.<name>]`.
pub fn configured_endpoints(
    providers: &BTreeMap<String, ProviderConfig>,
```

with:

```rust
            provider: b.name.to_string(),
            base_url: b.base_url.to_string(),
            api_key: None,
            protocol: b.protocol,
        })
        .collect()
}

/// Configured providers whose API key (if one is required) is present, plus any built-in
/// provider that needs a key (openrouter, openai, anthropic) whose key is set and that the user hasn't
/// redefined under `[providers.<name>]`.
pub fn configured_endpoints(
    providers: &BTreeMap<String, ProviderConfig>,
```

Replace (6 of 7):

```rust
                provider: name.clone(),
                base_url: cfg.base_url.clone(),
                api_key,
            })
        })
        .collect();
```

with:

```rust
                provider: name.clone(),
                base_url: cfg.base_url.clone(),
                api_key,
                protocol: cfg.protocol,
            })
        })
        .collect();
```

Replace (7 of 7):

```rust
                provider: builtin.name.to_string(),
                base_url: builtin.base_url.to_string(),
                api_key: Some(api_key),
            });
        }
    }
```

with:

```rust
                provider: builtin.name.to_string(),
                base_url: builtin.base_url.to_string(),
                api_key: Some(api_key),
                protocol: builtin.protocol,
            });
        }
    }
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p harness-providers -p harness-config`
Expected: PASS: 10 in `anthropic_messages_parser`, 4 in `anthropic_messages_http` (among them `both_context_overflow_errors_are_recognised`), 4 in `discovery`, 11 in `registry` and 34 in `config`; 99 tests in the two crates.

- [ ] **Step 7: Lint**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add crates/harness-config crates/harness-providers
git commit -F - <<'EOF'
feat(providers): add the Anthropic Messages adapter

The anthropic-messages protocol streams replies from POST /messages
with an API key in x-api-key: text, thinking as reasoning, tool use
emitted whole, usage counting cached input, and a Length finish at
max_tokens. Tool results and the user text after them share one user
message, empty messages are left out, and the system prompt and the
last message are cache breakpoints. Overloaded, API and rate-limit
errors inside the stream are retried like their HTTP forms. The
built-in anthropic provider uses ANTHROPIC_API_KEY, and harness models
lists Anthropic's models with its own headers.

<trailer lines from the controller>
EOF
```

---

### Task 4: Credentials, account profiles, and `auth add`, `auth use` and `logout`

**Files:**
- Create: `crates/harness-providers/src/credentials.rs`, `crates/harness-providers/tests/credentials.rs`, `crates/harness-cli/src/auth.rs`, `crates/harness-cli/tests/auth_e2e.rs`
- Modify: `Cargo.toml`, `Cargo.lock` (cargo updates it), `crates/harness-providers/Cargo.toml`, `crates/harness-providers/src/lib.rs`, `crates/harness-providers/src/registry.rs`, `crates/harness-providers/tests/registry.rs`, `crates/harness-cli/Cargo.toml`, `crates/harness-cli/src/main.rs`, `crates/harness-cli/src/setup.rs`, `crates/harness-cli/src/ask.rs`, `crates/harness-cli/src/slash.rs`, `crates/harness-cli/src/models.rs`, `crates/harness-cli/tests/cli_smoke.rs`, and the other end-to-end suites in `crates/harness-cli/tests/` (`ask_e2e.rs`, `checkpoints_e2e.rs`, `commands_e2e.rs`, `compaction_e2e.rs`, `context_e2e.rs`, `init_e2e.rs`, `sandbox_e2e.rs`, `sessions_e2e.rs`, `trust_e2e.rs`)

**Interfaces:**
- Consumes: `Builtin`, `Resolved` (Tasks 2 and 3), `Protocol::AnthropicMessages` (Task 3).
- Produces:
  - `harness_providers::credentials::{SERVICE = "harness", DEFAULT_PROFILE = "default", STORE_ENV = "HARNESS_CREDENTIAL_STORE", CredentialError, SecretStore, KeychainStore, FileStore, Credentials, check_name}`:
    - `trait SecretStore: Send + Sync { get(&self, account: &str) -> Result<Option<String>, CredentialError>; set(&self, account, secret) -> Result<(), CredentialError>; delete(&self, account) -> Result<bool, CredentialError>; describe(&self) -> String }`;
    - `KeychainStore::platform() -> Result<KeychainStore, CredentialError>` and `KeychainStore::with_store(Arc<keyring_core::CredentialStore>, name: &str)`;
    - `FileStore::new(data_dir)` (`credentials.json`);
    - `Credentials::open(data_dir, env: impl Fn(&str) -> Option<String>)`, `Credentials::with_keychain(data_dir, Option<Box<dyn SecretStore>>)`, and `take_warnings`, `active_profile(provider)`, `use_profile(provider, profile)`, `get(provider, profile)`, `active(provider)`, `set(provider, profile, secret) -> Result<String, _>` (returns where it went), `remove(provider, profile) -> Result<bool, _>`, all taking `&self`.
  - `registry::Secrets` (`env(var)`, `stored(provider)` defaulting to `None`), implemented by every `Fn(&str) -> Option<String>`; `resolve` and `configured_endpoints` take `impl Secrets`. `registry::is_claude_subscription_token(key) -> bool`. `Resolved` gains `api_key: Option<String>` and a `Debug` that hides it. `ResolveError` gains `SubscriptionToken { provider }`, and `MissingKey` says to run `harness auth add`.
  - In the CLI: `Setup::credentials` and `Setup::keys() -> setup::Keys` (the environment, then the store), and `auth::{add, use_profile, logout}`.

A stored key is looked up only for a provider with a key variable, so a request to a local server never reads the keychain. Every end-to-end test runs harness with `HARNESS_CREDENTIAL_STORE=file`, so no test reads the user's keychain either.

- [ ] **Step 1: Write the failing tests**

The providers' tests need a temporary directory. In `crates/harness-providers/Cargo.toml`:

Replace:

```toml
tokio.workspace = true

[dev-dependencies]
tokio.workspace = true
wiremock.workspace = true
```

with:

```toml
tokio.workspace = true

[dev-dependencies]
tempfile.workspace = true
tokio.workspace = true
wiremock.workspace = true
```

Create `crates/harness-providers/tests/credentials.rs`:

```rust
//! The credential store: keychain entries (through keyring-core's mock store, never the user's
//! real keychain), the `0600` file fallback, and account profiles.

use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

use harness_providers::credentials::{
    CredentialError, Credentials, DEFAULT_PROFILE, FileStore, KeychainStore, SecretStore,
};

fn mock_keychain() -> KeychainStore {
    KeychainStore::with_store(
        keyring_core::mock::Store::new().unwrap(),
        "the test keychain",
    )
}

/// A keychain that is there but refuses every operation, as a Secret Service without an
/// unlocked collection does.
struct Refusing;

impl SecretStore for Refusing {
    fn get(&self, _account: &str) -> Result<Option<String>, CredentialError> {
        Err(CredentialError::Keychain("the collection is locked".into()))
    }
    fn set(&self, _account: &str, _secret: &str) -> Result<(), CredentialError> {
        Err(CredentialError::Keychain("the collection is locked".into()))
    }
    fn delete(&self, _account: &str) -> Result<bool, CredentialError> {
        Err(CredentialError::Keychain("the collection is locked".into()))
    }
    fn describe(&self) -> String {
        "a locked keychain".into()
    }
}

/// Records what reaches it, to check what goes to the keychain.
#[derive(Clone, Default)]
struct Recording(Arc<Mutex<Vec<(String, String)>>>);

impl SecretStore for Recording {
    fn get(&self, account: &str) -> Result<Option<String>, CredentialError> {
        let entries = self.0.lock().unwrap();
        Ok(entries
            .iter()
            .rev()
            .find(|(a, _)| a == account)
            .map(|(_, s)| s.clone()))
    }
    fn set(&self, account: &str, secret: &str) -> Result<(), CredentialError> {
        self.0
            .lock()
            .unwrap()
            .push((account.to_string(), secret.to_string()));
        Ok(())
    }
    fn delete(&self, account: &str) -> Result<bool, CredentialError> {
        let mut entries = self.0.lock().unwrap();
        let before = entries.len();
        entries.retain(|(a, _)| a != account);
        Ok(entries.len() != before)
    }
    fn describe(&self) -> String {
        "the recording keychain".into()
    }
}

#[test]
fn keychain_entries_are_named_by_provider_and_profile() {
    let keychain = mock_keychain();
    assert_eq!(keychain.get("openai/default").unwrap(), None);
    keychain.set("openai/default", "sk-1").unwrap();
    assert_eq!(
        keychain.get("openai/default").unwrap().as_deref(),
        Some("sk-1")
    );
    assert_eq!(keychain.get("openai/work").unwrap(), None);
    assert!(keychain.delete("openai/default").unwrap());
    assert!(!keychain.delete("openai/default").unwrap());
    assert_eq!(keychain.get("openai/default").unwrap(), None);
}

#[test]
fn keys_go_to_the_keychain_and_never_to_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let keychain = Recording::default();
    let creds = Credentials::with_keychain(dir.path(), Some(Box::new(keychain.clone())));
    let stored = creds
        .set("openai", DEFAULT_PROFILE, "sk-in-keychain")
        .unwrap();
    assert_eq!(stored, "the recording keychain");
    assert!(creds.take_warnings().is_empty());
    assert_eq!(
        keychain.0.lock().unwrap().as_slice(),
        [("openai/default".to_string(), "sk-in-keychain".to_string())]
    );
    assert!(!dir.path().join("credentials.json").exists());
    assert_eq!(
        creds.active("openai").unwrap().as_deref(),
        Some("sk-in-keychain")
    );
}

// Spec: "No keychain on a headless Linux host".
#[test]
fn without_a_keychain_keys_go_to_a_private_file_with_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    for keychain in [None, Some(Box::new(Refusing) as Box<dyn SecretStore>)] {
        let creds = Credentials::with_keychain(&data, keychain);
        let stored = creds.set("openrouter", DEFAULT_PROFILE, "sk-or-1").unwrap();
        let file = data.join("credentials.json");
        assert_eq!(stored, file.display().to_string());
        let warnings = creds.take_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("no keychain"), "{warnings:?}");
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            creds.active("openrouter").unwrap().as_deref(),
            Some("sk-or-1")
        );
        std::fs::remove_file(file).unwrap();
    }
}

#[test]
fn a_key_stored_in_the_file_is_found_once_a_keychain_exists() {
    let dir = tempfile::tempdir().unwrap();
    Credentials::with_keychain(dir.path(), None)
        .set("openai", "work", "sk-file")
        .unwrap();
    let creds = Credentials::with_keychain(dir.path(), Some(Box::new(mock_keychain())));
    assert_eq!(
        creds.get("openai", "work").unwrap().as_deref(),
        Some("sk-file")
    );
    // Removing it removes it everywhere.
    assert!(creds.remove("openai", "work").unwrap());
    assert_eq!(creds.get("openai", "work").unwrap(), None);
    assert!(!creds.remove("openai", "work").unwrap());
}

#[test]
fn the_file_store_keeps_several_entries_and_rewrites_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path());
    store.set("a/default", "1").unwrap();
    store.set("b/default", "2").unwrap();
    store.set("a/default", "3").unwrap();
    assert_eq!(store.get("a/default").unwrap().as_deref(), Some("3"));
    assert_eq!(store.get("b/default").unwrap().as_deref(), Some("2"));
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.contains("tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

// Review Focus: a credentials file someone made readable by others is made private again.
#[test]
fn a_readable_credentials_file_is_made_private_again() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("credentials.json");
    std::fs::write(&file, r#"{"credentials":{"openai/default":"sk-1"}}"#).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let store = FileStore::new(dir.path());
    assert_eq!(
        store.get("openai/default").unwrap().as_deref(),
        Some("sk-1")
    );
    let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn a_damaged_credentials_file_is_an_error_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("credentials.json"), "{not json").unwrap();
    let error = FileStore::new(dir.path())
        .get("openai/default")
        .unwrap_err();
    assert!(error.to_string().contains("credentials.json"), "{error}");
}

// Spec: "Switching ChatGPT accounts".
#[test]
fn the_active_profile_is_remembered_per_provider() {
    let dir = tempfile::tempdir().unwrap();
    let creds = Credentials::with_keychain(dir.path(), Some(Box::new(mock_keychain())));
    creds.set("openai", DEFAULT_PROFILE, "sk-personal").unwrap();
    creds.set("openai", "work", "sk-work").unwrap();
    assert_eq!(creds.active_profile("openai").unwrap(), "default");
    creds.use_profile("openai", "work").unwrap();
    assert_eq!(creds.active_profile("anthropic").unwrap(), "default");
    // A new process reads the choice back.
    let reopened = Credentials::with_keychain(dir.path(), None);
    assert_eq!(reopened.active_profile("openai").unwrap(), "work");
    assert_eq!(creds.active("openai").unwrap().as_deref(), Some("sk-work"));
    let accounts = std::fs::read_to_string(dir.path().join("accounts.toml")).unwrap();
    assert!(!accounts.contains("sk-"), "{accounts}");
}

#[test]
fn provider_and_profile_names_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let creds = Credentials::with_keychain(dir.path(), None);
    for (provider, profile) in [("open/ai", "default"), ("openai", "../x"), ("openai", "")] {
        assert!(
            matches!(
                creds.set(provider, profile, "k"),
                Err(CredentialError::BadName { .. })
            ),
            "{provider} {profile}"
        );
    }
    assert!(creds.use_profile("openai", "a b").is_err());
}

#[test]
fn the_file_store_can_be_chosen_with_an_environment_variable() {
    let dir = tempfile::tempdir().unwrap();
    let creds = Credentials::open(dir.path(), |var| {
        (var == "HARNESS_CREDENTIAL_STORE").then(|| "file".to_string())
    });
    creds.set("openai", DEFAULT_PROFILE, "sk-1").unwrap();
    assert!(dir.path().join("credentials.json").exists());
    // Chosen, so no warning.
    assert!(creds.take_warnings().is_empty());
}
```

In `crates/harness-providers/tests/registry.rs`:

Replace (1 of 2):

```rust
use std::collections::{BTreeMap, HashMap};

use harness_config::config::{Protocol, ProviderConfig};
use harness_providers::registry::{ResolveError, configured_endpoints, local_endpoints, resolve};

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
```

with:

```rust
use std::collections::{BTreeMap, HashMap};

use harness_config::config::{Protocol, ProviderConfig};
use harness_providers::registry::{
    ResolveError, Secrets, configured_endpoints, local_endpoints, resolve,
};

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
```

Replace (2 of 2):

```rust
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].protocol, Protocol::AnthropicMessages);
}
```

with:

```rust
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].protocol, Protocol::AnthropicMessages);
}

/// Environment variables and stored keys, as the CLI gives them.
struct Keys {
    env: HashMap<String, String>,
    stored: HashMap<String, String>,
}

impl Keys {
    fn new(env: &[(&str, &str)], stored: &[(&str, &str)]) -> Keys {
        let map = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        Keys {
            env: map(env),
            stored: map(stored),
        }
    }
}

impl Secrets for Keys {
    fn env(&self, var: &str) -> Option<String> {
        self.env.get(var).cloned()
    }

    fn stored(&self, provider: &str) -> Option<String> {
        self.stored.get(provider).cloned()
    }
}

// Spec: "Environment variable wins".
#[test]
fn the_environment_wins_over_a_stored_key() {
    let none = BTreeMap::new();
    let stored = Keys::new(&[], &[("openai", "sk-stored")]);
    let r = resolve("openai/gpt-5", &none, stored).unwrap();
    assert_eq!(r.api_key.as_deref(), Some("sk-stored"));
    let both = Keys::new(&[("OPENAI_API_KEY", "sk-env")], &[("openai", "sk-stored")]);
    let r = resolve("openai/gpt-5", &none, both).unwrap();
    assert_eq!(r.api_key.as_deref(), Some("sk-env"));
    let found = configured_endpoints(&none, Keys::new(&[], &[("openai", "sk-stored")]));
    assert_eq!(found[0].api_key.as_deref(), Some("sk-stored"));
}

// A local server has no key variable: nothing stored is looked up for it, so a request to it
// never reads the keychain.
#[test]
fn a_provider_without_a_key_variable_takes_no_stored_key() {
    let providers = custom("local", "http://127.0.0.1:8000/v1", None);
    let r = resolve("local/m", &providers, Keys::new(&[], &[("local", "k")])).unwrap();
    assert_eq!(r.api_key, None);
    let r = resolve(
        "ollama/m",
        &BTreeMap::new(),
        Keys::new(&[], &[("ollama", "k")]),
    )
    .unwrap();
    assert_eq!(r.api_key, None);
}

#[test]
fn a_missing_key_says_how_to_add_one() {
    let error = resolve("openai/gpt-5", &BTreeMap::new(), env(&[])).unwrap_err();
    assert_eq!(
        error.to_string(),
        "provider `openai` needs an API key: set $OPENAI_API_KEY or run `harness auth add openai`"
    );
}

// Spec: "A subscription token in the environment".
#[test]
fn claude_subscription_tokens_are_refused_wherever_they_come_from() {
    let none = BTreeMap::new();
    let refused = ResolveError::SubscriptionToken {
        provider: "anthropic".into(),
    };
    let from_env = env(&[("ANTHROPIC_API_KEY", "sk-ant-oat01-abc")]);
    assert_eq!(
        resolve("anthropic/claude-sonnet-4-5", &none, from_env).err(),
        Some(refused.clone())
    );
    let stored = Keys::new(&[], &[("anthropic", "sk-ant-oat01-abc")]);
    assert_eq!(
        resolve("anthropic/claude-sonnet-4-5", &none, stored).err(),
        Some(refused.clone())
    );
    assert!(refused.to_string().contains("API key"), "{refused}");
}
```

Create `crates/harness-cli/tests/auth_e2e.rs`:

```rust
//! `harness auth add`, `auth use` and `logout`, and which key requests carry. Credentials go to
//! the file store (`HARNESS_CREDENTIAL_STORE=file`), so the tests never touch a real keychain.

use std::os::unix::fs::PermissionsExt;

use assert_cmd::Command;
use predicates::str::contains;
use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_harness");

fn answer(text: &str) -> ResponseTemplate {
    let chunk =
        json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": "stop"}]});
    ResponseTemplate::new(200).set_body_raw(
        format!("data: {chunk}\n\ndata: [DONE]\n\n"),
        "text/event-stream",
    )
}

/// Answers with the key a request carried.
async fn echo_keys(server: &MockServer) {
    for key in ["sk-stored", "sk-env", "sk-work"] {
        Mock::given(method("POST"))
            .and(header("authorization", format!("Bearer {key}").as_str()))
            .respond_with(answer(&format!("used {key}")))
            .mount(server)
            .await;
    }
}

struct Env {
    home: TempDir,
    ws: TempDir,
    user_home: TempDir,
}

impl Env {
    /// Provider `mock` at `server_uri` takes its key from `MOCK_API_KEY`; `extra` is more config.
    fn new(server_uri: &str, extra: &str) -> Env {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let user_home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(
            home.path().join("config/config.toml"),
            format!(
                "[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{server_uri}/v1\"\napi_key_env = \"MOCK_API_KEY\"\n{extra}"
            ),
        )
        .unwrap();
        std::fs::create_dir(ws.path().join(".git")).unwrap();
        Env {
            home,
            ws,
            user_home,
        }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.current_dir(self.ws.path())
            .env("HARNESS_HOME", self.home.path())
            .env("HOME", self.user_home.path())
            .env("HARNESS_CREDENTIAL_STORE", "file")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("MOCK_API_KEY")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY");
        cmd
    }

    fn add(&self, provider: &str, profile: Option<&str>, key: &str) -> assert_cmd::assert::Assert {
        let mut cmd = self.cmd();
        cmd.args(["auth", "add", provider]);
        if let Some(profile) = profile {
            cmd.args(["--profile", profile]);
        }
        cmd.write_stdin(key).assert()
    }

    fn credentials(&self) -> std::path::PathBuf {
        self.home.path().join("data/credentials.json")
    }
}

// Spec: "Environment variable wins", and a piped key keeps no trailing newline.
#[tokio::test(flavor = "multi_thread")]
async fn a_stored_key_is_used_unless_the_environment_has_one() {
    let server = MockServer::start().await;
    echo_keys(&server).await;
    let env = Env::new(&server.uri(), "");
    tokio::task::spawn_blocking(move || {
        env.add("mock", None, "sk-stored\n")
            .success()
            .stdout(contains("Stored the API key for mock (profile default)"));
        let file = env.credentials();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // Never in the configuration directory, which people sync to dotfile repositories.
        let config = std::fs::read_to_string(env.home.path().join("config/config.toml")).unwrap();
        assert!(!config.contains("sk-stored"));
        env.cmd()
            .args(["--model", "mock/m", "ask", "hi"])
            .assert()
            .success()
            .stdout(contains("used sk-stored"));
        env.cmd()
            .env("MOCK_API_KEY", "sk-env")
            .args(["--model", "mock/m", "ask", "hi"])
            .assert()
            .success()
            .stdout(contains("used sk-env"));
    })
    .await
    .unwrap();
}

// Spec: "Switching ChatGPT accounts", with an API-key provider; and "Logout".
#[tokio::test(flavor = "multi_thread")]
async fn profiles_are_chosen_with_auth_use_and_removed_with_logout() {
    let server = MockServer::start().await;
    echo_keys(&server).await;
    let env = Env::new(&server.uri(), "");
    tokio::task::spawn_blocking(move || {
        env.add("mock", None, "sk-stored").success();
        env.add("mock", Some("work"), "sk-work").success();
        env.cmd()
            .args(["auth", "use", "mock", "work"])
            .assert()
            .success()
            .stdout(contains("mock now uses profile work"));
        env.cmd()
            .args(["--model", "mock/m", "ask", "hi"])
            .assert()
            .success()
            .stdout(contains("used sk-work"));
        env.cmd()
            .args(["logout", "mock"])
            .assert()
            .success()
            .stdout(contains(
                "Removed the stored credentials for mock (profile work)",
            ));
        env.cmd()
            .args(["--model", "mock/m", "ask", "hi"])
            .assert()
            .code(2)
            .stderr(contains("harness auth add mock"));
        env.cmd()
            .args(["logout", "mock", "--profile", "work"])
            .assert()
            .success()
            .stdout(contains(
                "No credentials are stored for mock (profile work)",
            ));
        env.cmd()
            .args(["auth", "use", "mock", "default"])
            .assert()
            .success();
        env.cmd()
            .args(["--model", "mock/m", "ask", "hi"])
            .assert()
            .success()
            .stdout(contains("used sk-stored"));
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_add_refuses_what_it_cannot_store() {
    let server = MockServer::start().await;
    let env = Env::new(&server.uri(), "");
    tokio::task::spawn_blocking(move || {
        env.add("mock", None, "\n")
            .code(2)
            .stderr(contains("no API key"));
        env.add("nope", None, "k")
            .code(2)
            .stderr(contains("unknown provider `nope`"));
        env.add("ollama", None, "k")
            .code(2)
            .stderr(contains("needs no API key"));
        env.add("chatgpt", None, "k")
            .code(2)
            .stderr(contains("harness login chatgpt"));
        env.add("mock", Some("../x"), "k").code(2);
        assert!(!env.credentials().exists());
    })
    .await
    .unwrap();
}

/// A signed-in Claude Code in the fake home directory, with a canary for a token.
fn sign_in_claude_code(env: &Env) {
    let claude = env.user_home.path().join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(
        claude.join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-CANARY","refreshToken":"sk-ant-ort01-CANARY"}}"#,
    )
    .unwrap();
}

// Spec: "Claude Code signed in, no API key" and "A subscription token in the environment".
#[tokio::test(flavor = "multi_thread")]
async fn claude_subscription_credentials_are_never_used() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    // The built-in provider, pointed at the mock server so that nothing leaves the machine.
    let anthropic = format!(
        "[providers.anthropic]\nprotocol = \"anthropic-messages\"\nbase_url = \"{}/v1\"\napi_key_env = \"ANTHROPIC_API_KEY\"\n",
        server.uri()
    );
    let env = Env::new(&server.uri(), &anthropic);
    sign_in_claude_code(&env);
    let env = tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["--model", "anthropic/claude-sonnet-4-5", "ask", "hi"])
            .assert()
            .code(2)
            .stderr(contains("needs an API key"));
        env.cmd()
            .env("ANTHROPIC_API_KEY", "sk-ant-oat01-CANARY")
            .args(["--model", "anthropic/claude-sonnet-4-5", "ask", "hi"])
            .assert()
            .code(2)
            .stderr(contains("Claude subscription token"));
        env.add("anthropic", None, "sk-ant-oat01-CANARY")
            .code(2)
            .stderr(contains("Claude subscription token"));
        assert!(!env.credentials().exists());
        env
    })
    .await
    .unwrap();
    assert!(server.received_requests().await.unwrap().is_empty());
    drop(env);
}
```

In `crates/harness-cli/tests/cli_smoke.rs`, the help lists the new commands, and the session flags are refused with them as with the others (P3 made `-c` and `--resume` an error outside `ask`):

Replace (1 of 4):

```rust
        Command::new(env!("CARGO_BIN_EXE_harness"))
            .args(args)
            .env("HARNESS_HOME", home.path())
            .assert()
            .code(2)
            .stderr(contains("--resume needs an id"));
```

with:

```rust
        Command::new(env!("CARGO_BIN_EXE_harness"))
            .args(args)
            .env("HARNESS_HOME", home.path())
            .env("HARNESS_CREDENTIAL_STORE", "file")
            .assert()
            .code(2)
            .stderr(contains("--resume needs an id"));
```

Replace (2 of 4):

```rust
        Command::new(env!("CARGO_BIN_EXE_harness"))
            .args(args)
            .env("HARNESS_HOME", home.path())
            .assert()
            .code(2)
            .stderr(contains("--resume needs an id, followed by the prompt"));
```

with:

```rust
        Command::new(env!("CARGO_BIN_EXE_harness"))
            .args(args)
            .env("HARNESS_HOME", home.path())
            .env("HARNESS_CREDENTIAL_STORE", "file")
            .assert()
            .code(2)
            .stderr(contains("--resume needs an id, followed by the prompt"));
```

Replace (3 of 4):

```rust
        Command::new(env!("CARGO_BIN_EXE_harness"))
            .args(args)
            .env("HARNESS_HOME", home.path())
            .assert()
            .code(2)
            .stderr(contains(format!(
```

with:

```rust
        Command::new(env!("CARGO_BIN_EXE_harness"))
            .args(args)
            .env("HARNESS_HOME", home.path())
            .env("HARNESS_CREDENTIAL_STORE", "file")
            .assert()
            .code(2)
            .stderr(contains(format!(
```

Replace (4 of 4):

```rust
            )));
    }
}
```

with:

```rust
            )));
    }
}

// Spec: "Help output" lists the credential commands.
#[test]
fn help_lists_the_credential_commands() {
    Command::new(env!("CARGO_BIN_EXE_harness"))
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("auth").and(contains("logout")));
    Command::new(env!("CARGO_BIN_EXE_harness"))
        .args(["auth", "--help"])
        .assert()
        .success()
        .stdout(contains("add").and(contains("use")));
    Command::new(env!("CARGO_BIN_EXE_harness"))
        .args(["auth", "add", "--help"])
        .assert()
        .success()
        .stdout(contains("--profile").and(contains("standard input")));
}

#[test]
fn session_flags_with_the_credential_commands_are_refused() {
    let home = tempfile::tempdir().unwrap();
    for (args, command) in [
        (&["-c", "auth", "add", "openai"][..], "harness auth add"),
        (&["auth", "use", "openai", "work", "-c"], "harness auth use"),
        (&["logout", "openai", "--continue"], "harness logout"),
    ] {
        Command::new(env!("CARGO_BIN_EXE_harness"))
            .args(args)
            .env("HARNESS_HOME", home.path())
            .env("HARNESS_CREDENTIAL_STORE", "file")
            .write_stdin("sk-never-stored")
            .assert()
            .code(2)
            .stderr(contains(format!(
                "-c/--continue continues a session, which only `harness ask` does; run `{command}` without it"
            )));
    }
    assert!(!home.path().join("data/credentials.json").exists());
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p harness-providers --test credentials; cargo test -p harness-providers --test registry; cargo test -p harness-cli --test auth_e2e; cargo test -p harness-cli --test cli_smoke`
Expected: `credentials` FAILS to compile (``unresolved import `harness_providers::credentials` ``, ``cannot find module or crate `keyring_core` ``), and so does `registry` (``unresolved import `harness_providers::registry::Secrets` ``). `auth_e2e` FAILS all 4 tests: three with `error: unrecognized subcommand 'auth'`, and `claude_subscription_credentials_are_never_used` because the run with `ANTHROPIC_API_KEY=sk-ant-oat01-CANARY` exits 1, not 2: the subscription token was sent to the (mock) server. `cli_smoke` FAILS `help_lists_the_credential_commands` and `session_flags_with_the_credential_commands_are_refused` (`unrecognized subcommand 'auth'`); its other 6 pass.

- [ ] **Step 3: Add the dependencies**

In `Cargo.toml`:

Replace:

```toml
landlock = "0.4.7"
libc = "0.2"
jsonschema = "0.57.0"
nix = { version = "0.31.3", features = ["signal", "process"] }
regex = "1"
reqwest = { version = "0.13.5", default-features = false, features = ["json", "stream", "rustls"] }
```

with:

```toml
landlock = "0.4.7"
libc = "0.2"
jsonschema = "0.57.0"
keyring-core = "1.0.0"
apple-native-keyring-store = { version = "1.0.2", features = ["keychain"] }
zbus-secret-service-keyring-store = { version = "1.0.1", features = ["rt-async-io-crypto-rust"] }
nix = { version = "0.31.3", features = ["signal", "process"] }
regex = "1"
reqwest = { version = "0.13.5", default-features = false, features = ["json", "stream", "rustls"] }
```

In `crates/harness-providers/Cargo.toml`:

Replace:

```toml
futures.workspace = true
harness-config.workspace = true
harness-core.workspace = true
reqwest.workspace = true
serde_json.workspace = true
thiserror.workspace = true
tokio.workspace = true

[dev-dependencies]
tempfile.workspace = true
```

with:

```toml
futures.workspace = true
harness-config.workspace = true
harness-core.workspace = true
keyring-core.workspace = true
reqwest.workspace = true
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
tokio.workspace = true
toml.workspace = true

[target.'cfg(target_os = "macos")'.dependencies]
apple-native-keyring-store.workspace = true

[target.'cfg(target_os = "linux")'.dependencies]
zbus-secret-service-keyring-store.workspace = true

[dev-dependencies]
tempfile.workspace = true
```

- [ ] **Step 4: Implement the credential store**

Keys go to the keychain, and to the file only when the keychain refuses or there is none. A file chosen with `HARNESS_CREDENTIAL_STORE=file` gets no warning. The keychain runs each operation through `keyring-core`'s store object rather than its global default store, so tests can hand it the mock store.

Create `crates/harness-providers/src/credentials.rs`:

```rust
//! Where API keys and sign-in tokens are kept: the OS keychain (the macOS Keychain, or the Secret
//! Service on Linux), or `credentials.json` in the data directory, mode `0600`, when no keychain
//! can be used. Each credential belongs to a provider and an account profile, and is stored under
//! the account name `<provider>/<profile>`. Which profile a provider uses is recorded in
//! `accounts.toml` next to it. Nothing here is ever written to the configuration directory.

use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};

/// The keychain service every harness credential is stored under.
pub const SERVICE: &str = "harness";
/// The profile used when none is named.
pub const DEFAULT_PROFILE: &str = "default";
/// `file` keeps credentials in the file only, never in the keychain.
pub const STORE_ENV: &str = "HARNESS_CREDENTIAL_STORE";

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("the keychain refused: {0}")]
    Keychain(String),
    #[error("cannot use {}: {source}", .path.display())]
    File {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{} is damaged: {message}", .path.display())]
    Damaged { path: PathBuf, message: String },
    #[error("invalid {what} name `{name}`: use letters, digits, `.`, `_` and `-`")]
    BadName { what: &'static str, name: String },
}

/// Somewhere secrets can be kept, by account name.
pub trait SecretStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<String>, CredentialError>;
    fn set(&self, account: &str, secret: &str) -> Result<(), CredentialError>;
    /// Whether there was something to delete.
    fn delete(&self, account: &str) -> Result<bool, CredentialError>;
    /// Where the secrets are kept, for messages.
    fn describe(&self) -> String;
}

/// The OS keychain, through a `keyring-core` credential store.
pub struct KeychainStore {
    store: Arc<keyring_core::CredentialStore>,
    name: String,
}

impl KeychainStore {
    /// The platform's keychain: the macOS Keychain, or the Secret Service over D-Bus on Linux.
    /// An error means there is none to use (no session bus, say).
    pub fn platform() -> Result<KeychainStore, CredentialError> {
        #[cfg(target_os = "macos")]
        let (store, name): (Arc<keyring_core::CredentialStore>, _) = (
            apple_native_keyring_store::keychain::Store::new().map_err(keychain_error)?,
            "the macOS keychain",
        );
        #[cfg(target_os = "linux")]
        let (store, name): (Arc<keyring_core::CredentialStore>, _) = (
            zbus_secret_service_keyring_store::Store::new().map_err(keychain_error)?,
            "the Secret Service keyring",
        );
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        return Err(CredentialError::Keychain(
            "no keychain is supported on this system".into(),
        ));
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        Ok(KeychainStore::with_store(store, name))
    }

    /// A keychain backed by `store` (tests use keyring-core's mock store).
    pub fn with_store(store: Arc<keyring_core::CredentialStore>, name: &str) -> KeychainStore {
        KeychainStore {
            store,
            name: name.to_string(),
        }
    }

    fn entry(&self, account: &str) -> Result<keyring_core::Entry, CredentialError> {
        self.store
            .build(SERVICE, account, None)
            .map_err(keychain_error)
    }
}

fn keychain_error(error: keyring_core::Error) -> CredentialError {
    CredentialError::Keychain(error.to_string())
}

impl SecretStore for KeychainStore {
    fn get(&self, account: &str) -> Result<Option<String>, CredentialError> {
        match self.entry(account)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(keychain_error(e)),
        }
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), CredentialError> {
        self.entry(account)?
            .set_password(secret)
            .map_err(keychain_error)
    }

    fn delete(&self, account: &str) -> Result<bool, CredentialError> {
        match self.entry(account)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring_core::Error::NoEntry) => Ok(false),
            Err(e) => Err(keychain_error(e)),
        }
    }

    fn describe(&self) -> String {
        self.name.clone()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CredentialsFile {
    #[serde(default)]
    credentials: BTreeMap<String, String>,
}

/// `credentials.json` in the data directory: readable and writable by its owner only, rewritten
/// whole through a temporary file, and locked while it is read and rewritten.
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(data_dir: &Path) -> FileStore {
        FileStore {
            path: data_dir.join("credentials.json"),
        }
    }

    fn io(&self, source: std::io::Error) -> CredentialError {
        CredentialError::File {
            path: self.path.clone(),
            source,
        }
    }

    /// Locks the file against other harness processes until the returned guard is dropped.
    fn lock(&self) -> Result<File, CredentialError> {
        let dir = self.path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).map_err(|e| self.io(e))?;
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.path.with_extension("json.lock"))
            .map_err(|e| self.io(e))?;
        lock.lock().map_err(|e| self.io(e))?;
        Ok(lock)
    }

    fn read(&self) -> Result<CredentialsFile, CredentialError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CredentialsFile::default());
            }
            Err(e) => return Err(self.io(e)),
        };
        // A file someone made readable by others is made private again.
        if let Ok(metadata) = std::fs::metadata(&self.path)
            && metadata.permissions().mode() & 0o077 != 0
        {
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| self.io(e))?;
        }
        serde_json::from_str(&text).map_err(|e| CredentialError::Damaged {
            path: self.path.clone(),
            message: e.to_string(),
        })
    }

    fn write(&self, file: &CredentialsFile) -> Result<(), CredentialError> {
        let text = serde_json::to_string_pretty(file).expect("credentials serialize");
        let tmp = self
            .path
            .with_extension(format!("json.tmp-{}", std::process::id()));
        let written = (|| {
            let mut out = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            out.write_all(text.as_bytes())?;
            out.sync_all()?;
            std::fs::rename(&tmp, &self.path)
        })();
        written.map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            self.io(e)
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl SecretStore for FileStore {
    fn get(&self, account: &str) -> Result<Option<String>, CredentialError> {
        let _lock = self.lock()?;
        Ok(self.read()?.credentials.remove(account))
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), CredentialError> {
        let _lock = self.lock()?;
        let mut file = self.read()?;
        file.credentials
            .insert(account.to_string(), secret.to_string());
        self.write(&file)
    }

    fn delete(&self, account: &str) -> Result<bool, CredentialError> {
        if !self.path.exists() {
            return Ok(false);
        }
        let _lock = self.lock()?;
        let mut file = self.read()?;
        let removed = file.credentials.remove(account).is_some();
        if removed {
            self.write(&file)?;
        }
        Ok(removed)
    }

    fn describe(&self) -> String {
        self.path.display().to_string()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AccountsFile {
    /// The profile each provider uses, when it is not `default`.
    #[serde(default)]
    active: BTreeMap<String, String>,
}

/// Stored credentials by provider and account profile.
pub struct Credentials {
    keychain: Option<Box<dyn SecretStore>>,
    /// Why there is no keychain, when it was not left out on purpose.
    no_keychain: Option<String>,
    file: FileStore,
    accounts: PathBuf,
    warnings: Mutex<Vec<String>>,
}

impl Credentials {
    /// The credentials for `data_dir`: in the OS keychain when there is one, unless `STORE_ENV`
    /// (read through `env`) is `file`.
    pub fn open(data_dir: &Path, env: impl Fn(&str) -> Option<String>) -> Credentials {
        if env(STORE_ENV).as_deref() == Some("file") {
            let mut credentials = Credentials::with_keychain(data_dir, None);
            credentials.no_keychain = None;
            return credentials;
        }
        match KeychainStore::platform() {
            Ok(keychain) => Credentials::with_keychain(data_dir, Some(Box::new(keychain))),
            Err(e) => {
                let mut credentials = Credentials::with_keychain(data_dir, None);
                credentials.no_keychain = Some(e.to_string());
                credentials
            }
        }
    }

    /// The credentials for `data_dir`, with `keychain` as the keychain. Without one, credentials
    /// go to the file with a warning.
    pub fn with_keychain(data_dir: &Path, keychain: Option<Box<dyn SecretStore>>) -> Credentials {
        Credentials {
            no_keychain: keychain.is_none().then(|| "none is available".to_string()),
            keychain,
            file: FileStore::new(data_dir),
            accounts: data_dir.join("accounts.toml"),
            warnings: Mutex::new(Vec::new()),
        }
    }

    /// Warnings gathered since the last call: that a credential went to the file.
    pub fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut *self.warnings.lock().expect("warnings lock"))
    }

    fn read_accounts(&self) -> Result<AccountsFile, CredentialError> {
        match std::fs::read_to_string(&self.accounts) {
            Ok(text) => toml::from_str(&text).map_err(|e| CredentialError::Damaged {
                path: self.accounts.clone(),
                message: e.to_string(),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AccountsFile::default()),
            Err(source) => Err(CredentialError::File {
                path: self.accounts.clone(),
                source,
            }),
        }
    }

    /// The profile `provider` uses: the one chosen with `harness auth use`, or `default`.
    pub fn active_profile(&self, provider: &str) -> Result<String, CredentialError> {
        Ok(self
            .read_accounts()?
            .active
            .remove(provider)
            .unwrap_or_else(|| DEFAULT_PROFILE.to_string()))
    }

    /// Makes `profile` the one `provider` uses from now on.
    pub fn use_profile(&self, provider: &str, profile: &str) -> Result<(), CredentialError> {
        check_name("provider", provider)?;
        check_name("profile", profile)?;
        let mut accounts = self.read_accounts()?;
        if profile == DEFAULT_PROFILE {
            accounts.active.remove(provider);
        } else {
            accounts
                .active
                .insert(provider.to_string(), profile.to_string());
        }
        let io = |source| CredentialError::File {
            path: self.accounts.clone(),
            source,
        };
        let text = toml::to_string(&accounts).expect("accounts serialize");
        if let Some(dir) = self.accounts.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let tmp = self
            .accounts
            .with_extension(format!("toml.tmp-{}", std::process::id()));
        let written = (|| {
            let mut out = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            out.write_all(text.as_bytes())?;
            std::fs::rename(&tmp, &self.accounts)
        })();
        written.map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            io(e)
        })
    }

    /// The credential stored for `provider` under `profile`: from the keychain, or else the
    /// file (where it went when no keychain could be used). A keychain that refuses to answer
    /// counts as holding nothing.
    pub fn get(&self, provider: &str, profile: &str) -> Result<Option<String>, CredentialError> {
        let account = account(provider, profile)?;
        if let Some(keychain) = &self.keychain
            && let Ok(Some(secret)) = keychain.get(&account)
        {
            return Ok(Some(secret));
        }
        self.file.get(&account)
    }

    /// The credential of the profile `provider` uses.
    pub fn active(&self, provider: &str) -> Result<Option<String>, CredentialError> {
        let profile = self.active_profile(provider)?;
        self.get(provider, &profile)
    }

    /// Stores `secret` for `provider` under `profile`, in the keychain when one works, else in
    /// the file with a warning. Returns where it went.
    pub fn set(
        &self,
        provider: &str,
        profile: &str,
        secret: &str,
    ) -> Result<String, CredentialError> {
        let account = account(provider, profile)?;
        // Why the file is used; nothing to say when it was chosen (`HARNESS_CREDENTIAL_STORE`).
        let why = match &self.keychain {
            Some(keychain) => match keychain.set(&account, secret) {
                Ok(()) => {
                    // An older copy in the file must not outlive this one.
                    let _ = self.file.delete(&account);
                    return Ok(keychain.describe());
                }
                Err(e) => Some(e.to_string()),
            },
            None => self.no_keychain.clone(),
        };
        self.file.set(&account, secret)?;
        if let Some(why) = why {
            self.warnings.lock().expect("warnings lock").push(format!(
                "no keychain could store it ({why}); it is in {}, readable only by you",
                self.file.describe()
            ));
        }
        Ok(self.file.describe())
    }

    /// Removes what is stored for `provider` under `profile`, from the keychain and the file.
    pub fn remove(&self, provider: &str, profile: &str) -> Result<bool, CredentialError> {
        let account = account(provider, profile)?;
        let in_keychain = match &self.keychain {
            Some(keychain) => keychain.delete(&account).unwrap_or(false),
            None => false,
        };
        let in_file = self.file.delete(&account)?;
        Ok(in_keychain || in_file)
    }
}

/// The account name of a provider's profile.
fn account(provider: &str, profile: &str) -> Result<String, CredentialError> {
    check_name("provider", provider)?;
    check_name("profile", profile)?;
    Ok(format!("{provider}/{profile}"))
}

/// Checks a provider or profile name: letters, digits, `.`, `_` and `-`, at most 64.
pub fn check_name(what: &'static str, name: &str) -> Result<(), CredentialError> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if valid {
        Ok(())
    } else {
        Err(CredentialError::BadName {
            what,
            name: name.to_string(),
        })
    }
}
```

In `crates/harness-providers/src/lib.rs`:

Replace:

```rust
//! Model provider adapters, local-server discovery, and model-id resolution.

pub mod anthropic_messages;
pub mod discovery;
pub mod openai_chat;
pub mod openai_responses;
```

with:

```rust
//! Model provider adapters, local-server discovery, and model-id resolution.

pub mod anthropic_messages;
pub mod credentials;
pub mod discovery;
pub mod openai_chat;
pub mod openai_responses;
```

- [ ] **Step 5: Resolve keys from the environment, then the store**

In `crates/harness-providers/src/registry.rs`:

Replace (1 of 7):

```rust
    BadId(String),
    #[error("unknown provider `{0}`; define it under [providers.{0}] in config.toml")]
    UnknownProvider(String),
    #[error("provider `{provider}` needs an API key in ${var}")]
    MissingKey { provider: String, var: String },
}

/// A ready-to-use provider for one model id.
```

with:

```rust
    BadId(String),
    #[error("unknown provider `{0}`; define it under [providers.{0}] in config.toml")]
    UnknownProvider(String),
    #[error(
        "provider `{provider}` needs an API key: set ${var} or run `harness auth add {provider}`"
    )]
    MissingKey { provider: String, var: String },
    #[error(
        "the key for `{provider}` is a Claude subscription token, which only Claude Code may use; harness needs an Anthropic API key (from console.anthropic.com)"
    )]
    SubscriptionToken { provider: String },
}

/// Where API keys come from: environment variables, and keys stored with `harness auth add`.
/// A closure over environment variables is a `Secrets` with nothing stored.
pub trait Secrets {
    fn env(&self, var: &str) -> Option<String>;
    /// The key stored for `provider`'s active profile.
    fn stored(&self, _provider: &str) -> Option<String> {
        None
    }
}

impl<F: Fn(&str) -> Option<String>> Secrets for F {
    fn env(&self, var: &str) -> Option<String> {
        self(var)
    }
}

/// Whether `key` is a Claude subscription (OAuth) token rather than an API key.
pub fn is_claude_subscription_token(key: &str) -> bool {
    key.trim_start().starts_with("sk-ant-oat")
}

/// The key for provider `name`, whose key is in the environment variable `key_env`: that
/// variable, else the stored key. A provider without a key variable takes no key, and nothing
/// stored is looked up for it.
fn api_key(name: &str, key_env: Option<&str>, secrets: &impl Secrets) -> Option<String> {
    let var = key_env?;
    secrets
        .env(var)
        .filter(|v| !v.is_empty())
        .or_else(|| secrets.stored(name).filter(|v| !v.is_empty()))
}

/// A ready-to-use provider for one model id.
```

Replace (2 of 7):

```rust
    pub id: String,
    pub protocol: Protocol,
    pub base_url: String,
}

pub fn resolve(
    model_id: &str,
    providers: &BTreeMap<String, ProviderConfig>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Resolved, ResolveError> {
    let (name, model) = model_id
        .split_once('/')
```

with:

```rust
    pub id: String,
    pub protocol: Protocol,
    pub base_url: String,
    /// The API key requests carry, if any.
    pub api_key: Option<String>,
}

/// Leaves the key out, so that it never reaches a log or an error message.
impl std::fmt::Debug for Resolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolved")
            .field("id", &self.id)
            .field("protocol", &self.protocol)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

pub fn resolve(
    model_id: &str,
    providers: &BTreeMap<String, ProviderConfig>,
    secrets: impl Secrets,
) -> Result<Resolved, ResolveError> {
    let (name, model) = model_id
        .split_once('/')
```

Replace (3 of 7):

```rust
    } else {
        return Err(ResolveError::UnknownProvider(name.to_string()));
    };
    let api_key =
        match key_env {
            Some(var) => Some(env(&var).filter(|v| !v.is_empty()).ok_or(
                ResolveError::MissingKey {
                    provider: name.to_string(),
                    var,
                },
            )?),
            None => None,
        };
    let provider: Arc<dyn Provider> = match protocol {
        Protocol::OpenaiChat => Arc::new(OpenAiChat::new(base_url.clone(), api_key)),
        Protocol::OpenaiResponses => Arc::new(OpenAiResponses::new(base_url.clone(), api_key)),
        Protocol::AnthropicMessages => Arc::new(AnthropicMessages::new(base_url.clone(), api_key)),
    };
    Ok(Resolved {
        provider,
```

with:

```rust
    } else {
        return Err(ResolveError::UnknownProvider(name.to_string()));
    };
    let api_key = api_key(name, key_env.as_deref(), &secrets);
    if let (None, Some(var)) = (&api_key, key_env) {
        return Err(ResolveError::MissingKey {
            provider: name.to_string(),
            var,
        });
    }
    // Claude Free/Pro/Max credentials may only be used by Claude Code itself.
    if protocol == Protocol::AnthropicMessages
        && api_key.as_deref().is_some_and(is_claude_subscription_token)
    {
        return Err(ResolveError::SubscriptionToken {
            provider: name.to_string(),
        });
    }
    let provider: Arc<dyn Provider> = match protocol {
        Protocol::OpenaiChat => Arc::new(OpenAiChat::new(base_url.clone(), api_key.clone())),
        Protocol::OpenaiResponses => {
            Arc::new(OpenAiResponses::new(base_url.clone(), api_key.clone()))
        }
        Protocol::AnthropicMessages => {
            Arc::new(AnthropicMessages::new(base_url.clone(), api_key.clone()))
        }
    };
    Ok(Resolved {
        provider,
```

Replace (4 of 7):

```rust
        id: model_id.to_string(),
        protocol,
        base_url,
    })
}

```

with:

```rust
        id: model_id.to_string(),
        protocol,
        base_url,
        api_key,
    })
}

```

Replace (5 of 7):

```rust
/// redefined under `[providers.<name>]`.
pub fn configured_endpoints(
    providers: &BTreeMap<String, ProviderConfig>,
    env: impl Fn(&str) -> Option<String>,
) -> Vec<Endpoint> {
    let mut endpoints: Vec<Endpoint> = providers
        .iter()
        .filter_map(|(name, cfg)| {
            let api_key = match &cfg.api_key_env {
                Some(var) => Some(env(var).filter(|v| !v.is_empty())?),
                None => None,
            };
            Some(Endpoint {
                provider: name.clone(),
                base_url: cfg.base_url.clone(),
```

with:

```rust
/// redefined under `[providers.<name>]`.
pub fn configured_endpoints(
    providers: &BTreeMap<String, ProviderConfig>,
    secrets: impl Secrets,
) -> Vec<Endpoint> {
    let mut endpoints: Vec<Endpoint> = providers
        .iter()
        .filter_map(|(name, cfg)| {
            let api_key = api_key(name, cfg.api_key_env.as_deref(), &secrets);
            if cfg.api_key_env.is_some() && api_key.is_none() {
                return None;
            }
            Some(Endpoint {
                provider: name.clone(),
                base_url: cfg.base_url.clone(),
```

Replace (6 of 7):

```rust
        if LOCAL_PROVIDERS.contains(&builtin.name) || providers.contains_key(builtin.name) {
            continue;
        }
        if let Some(api_key) = builtin.key_env.and_then(&env).filter(|v| !v.is_empty()) {
            endpoints.push(Endpoint {
                provider: builtin.name.to_string(),
                base_url: builtin.base_url.to_string(),
```

with:

```rust
        if LOCAL_PROVIDERS.contains(&builtin.name) || providers.contains_key(builtin.name) {
            continue;
        }
        if builtin.key_env.is_none() {
            continue;
        }
        if let Some(api_key) = api_key(builtin.name, builtin.key_env, &secrets) {
            endpoints.push(Endpoint {
                provider: builtin.name.to_string(),
                base_url: builtin.base_url.to_string(),
```

Replace (7 of 7):

```rust
            });
        }
    }
    endpoints
}
```

with:

```rust
            });
        }
    }
    // A Claude subscription token is never sent anywhere, not even to list models.
    endpoints.retain(|e| {
        e.protocol != Protocol::AnthropicMessages
            || !e
                .api_key
                .as_deref()
                .is_some_and(is_claude_subscription_token)
    });
    endpoints
}
```

- [ ] **Step 6: Add `auth add`, `auth use` and `logout`**

Reading a key without echo needs `nix`'s terminal functions. In `crates/harness-cli/Cargo.toml`:

Replace:

```toml
harness-providers.workspace = true
harness-sandbox.workspace = true
harness-tools.workspace = true
serde_json.workspace = true
tokio.workspace = true
tokio-util.workspace = true
```

with:

```toml
harness-providers.workspace = true
harness-sandbox.workspace = true
harness-tools.workspace = true
nix = { workspace = true, features = ["term"] }
serde_json.workspace = true
tokio.workspace = true
tokio-util.workspace = true
```

Create `crates/harness-cli/src/auth.rs`:

```rust
//! `harness auth add`, `harness auth use` and `harness logout`: stored API keys and account
//! profiles. A key is read from standard input, never from the command line, where it would
//! reach the shell history.

use std::io::{BufRead, IsTerminal, Read};

use harness_config::config::Protocol;
use harness_providers::{
    credentials::{self, CredentialError},
    registry::{self, BUILTIN_PROVIDERS, ResolveError},
};

use crate::{
    setup::{self, Setup},
    term::terminal_safe,
};

/// What a provider needs to be used.
enum Needs {
    /// An API key, sent over this protocol.
    Key(Protocol),
    /// Signing in (`harness login`).
    SignIn,
    /// Nothing: a local server.
    Nothing,
}

/// What `provider` needs, or `None` when there is no such provider.
fn needs(setup: &Setup, provider: &str) -> Option<Needs> {
    if let Some(cfg) = setup.config.providers.get(provider) {
        return Some(match cfg.api_key_env {
            Some(_) => Needs::Key(cfg.protocol),
            None => Needs::Nothing,
        });
    }
    if provider == "chatgpt" {
        return Some(Needs::SignIn);
    }
    let builtin = BUILTIN_PROVIDERS.iter().find(|b| b.name == provider)?;
    Some(match builtin.key_env {
        Some(_) => Needs::Key(builtin.protocol),
        None => Needs::Nothing,
    })
}

fn load() -> Result<Setup, u8> {
    setup::load().map_err(|message| {
        eprintln!("error: {}", terminal_safe(&message));
        2
    })
}

/// Checks the provider and profile names, printing why one is not valid.
fn check_names(provider: &str, profile: &str) -> Result<(), u8> {
    credentials::check_name("provider", provider)
        .and_then(|()| credentials::check_name("profile", profile))
        .map_err(|e| {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
            2
        })
}

fn unknown(provider: &str) -> u8 {
    eprintln!(
        "error: unknown provider `{}`; define it under [providers.{}] in config.toml",
        terminal_safe(provider),
        terminal_safe(provider)
    );
    2
}

/// `harness auth add <provider> [--profile <name>]`.
pub fn add(provider: &str, profile: &str) -> u8 {
    let setup = match load() {
        Ok(setup) => setup,
        Err(code) => return code,
    };
    if let Err(code) = check_names(provider, profile) {
        return code;
    }
    let protocol = match needs(&setup, provider) {
        None => return unknown(provider),
        Some(Needs::SignIn) => {
            eprintln!(
                "error: {provider} takes no API key; sign in with `harness login {provider}`"
            );
            return 2;
        }
        Some(Needs::Nothing) => {
            eprintln!(
                "error: {provider} needs no API key; to give it one, set `api_key_env` under [providers.{provider}] in config.toml"
            );
            return 2;
        }
        Some(Needs::Key(protocol)) => protocol,
    };
    let key = match read_key(provider) {
        Ok(key) => key,
        Err(e) => {
            eprintln!("error: cannot read the key: {e}");
            return 1;
        }
    };
    if key.is_empty() {
        eprintln!("error: no API key was given on standard input");
        return 2;
    }
    if protocol == Protocol::AnthropicMessages && registry::is_claude_subscription_token(&key) {
        let refused = ResolveError::SubscriptionToken {
            provider: provider.to_string(),
        };
        eprintln!("error: {refused}");
        return 2;
    }
    match setup.credentials.set(provider, profile, &key) {
        Ok(place) => {
            for warning in setup.credentials.take_warnings() {
                eprintln!("warning: {}", terminal_safe(&warning));
            }
            println!(
                "Stored the API key for {provider} (profile {profile}) in {}.",
                terminal_safe(&place)
            );
            0
        }
        Err(e) => fail(e),
    }
}

/// `harness auth use <provider> <profile>`.
pub fn use_profile(provider: &str, profile: &str) -> u8 {
    let setup = match load() {
        Ok(setup) => setup,
        Err(code) => return code,
    };
    if let Err(code) = check_names(provider, profile) {
        return code;
    }
    let needs = match needs(&setup, provider) {
        None => return unknown(provider),
        Some(needs) => needs,
    };
    if let Err(e) = setup.credentials.use_profile(provider, profile) {
        return fail(e);
    }
    println!("{provider} now uses profile {profile}.");
    if let Ok(None) = setup.credentials.get(provider, profile) {
        let how = match needs {
            Needs::SignIn => format!("harness login {provider} --profile {profile}"),
            _ => format!("harness auth add {provider} --profile {profile}"),
        };
        println!("Nothing is stored under it yet: run `{how}`.");
    }
    0
}

/// `harness logout <provider> [--profile <name>]`: the named profile, or the one in use.
pub fn logout(provider: &str, profile: Option<&str>) -> u8 {
    let setup = match load() {
        Ok(setup) => setup,
        Err(code) => return code,
    };
    let profile = match profile {
        Some(profile) => profile.to_string(),
        None => match setup.credentials.active_profile(provider) {
            Ok(profile) => profile,
            Err(e) => return fail(e),
        },
    };
    if let Err(code) = check_names(provider, &profile) {
        return code;
    }
    match setup.credentials.remove(provider, &profile) {
        Ok(true) => {
            println!("Removed the stored credentials for {provider} (profile {profile}).");
            0
        }
        Ok(false) => {
            println!("No credentials are stored for {provider} (profile {profile}).");
            0
        }
        Err(e) => fail(e),
    }
}

fn fail(error: CredentialError) -> u8 {
    eprintln!("error: {}", terminal_safe(&error.to_string()));
    match error {
        CredentialError::BadName { .. } => 2,
        _ => 1,
    }
}

/// The key on standard input: typed without echo on a terminal, or the first line of what is
/// piped in (a password manager may print more lines after it), without surrounding whitespace.
fn read_key(provider: &str) -> std::io::Result<String> {
    let stdin = std::io::stdin();
    let text = if stdin.is_terminal() {
        eprint!("API key for {provider} (not shown): ");
        let line = read_hidden_line()?;
        eprintln!();
        line
    } else {
        let mut text = String::new();
        stdin.lock().read_to_string(&mut text)?;
        text
    };
    Ok(text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string())
}

/// One line from the terminal, with echo turned off while it is typed.
fn read_hidden_line() -> std::io::Result<String> {
    use nix::sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr};
    let stdin = std::io::stdin();
    let saved = tcgetattr(&stdin)?;
    let mut quiet = saved.clone();
    quiet.local_flags.remove(LocalFlags::ECHO);
    tcsetattr(&stdin, SetArg::TCSANOW, &quiet)?;
    let mut line = String::new();
    let read = stdin.lock().read_line(&mut line);
    let _ = tcsetattr(&stdin, SetArg::TCSANOW, &saved);
    read.map(|_| line)
}
```

In `crates/harness-cli/src/setup.rs`:

Replace (1 of 3):

```rust
    paths::Paths,
    trust::TrustStore,
};

/// Everything a command needs about where it runs.
pub struct Setup {
```

with:

```rust
    paths::Paths,
    trust::TrustStore,
};
use harness_providers::{credentials::Credentials, registry::Secrets};

/// Everything a command needs about where it runs.
pub struct Setup {
```

Replace (2 of 3):

```rust
    pub workspace: PathBuf,
    /// The workspaces the user trusts.
    pub trust: TrustStore,
}

/// Loads paths and configuration. Errors are user-facing messages (exit code 2).
```

with:

```rust
    pub workspace: PathBuf,
    /// The workspaces the user trusts.
    pub trust: TrustStore,
    /// Stored API keys and sign-in tokens.
    pub credentials: Credentials,
}

impl Setup {
    /// API keys from the environment, then from the credential store.
    pub fn keys(&self) -> Keys<'_> {
        Keys {
            credentials: &self.credentials,
        }
    }
}

/// API keys from the environment, then from the credential store (`harness auth add`).
#[derive(Clone, Copy)]
pub struct Keys<'a> {
    credentials: &'a Credentials,
}

impl Secrets for Keys<'_> {
    fn env(&self, var: &str) -> Option<String> {
        env(var)
    }

    fn stored(&self, provider: &str) -> Option<String> {
        match self.credentials.active(provider) {
            Ok(key) => key,
            Err(e) => {
                eprintln!(
                    "warning: cannot read the stored credentials: {}",
                    crate::term::terminal_safe(&e.to_string())
                );
                None
            }
        }
    }
}

/// Loads paths and configuration. Errors are user-facing messages (exit code 2).
```

Replace (3 of 3):

```rust
    for warning in &config.warnings {
        eprintln!("warning: {}", crate::term::terminal_safe(warning));
    }
    Ok(Setup {
        paths,
        config,
        workspace,
        trust,
    })
}

```

with:

```rust
    for warning in &config.warnings {
        eprintln!("warning: {}", crate::term::terminal_safe(warning));
    }
    let credentials = Credentials::open(&paths.data_dir, env);
    Ok(Setup {
        paths,
        config,
        workspace,
        trust,
        credentials,
    })
}

```

In `crates/harness-cli/src/main.rs`:

Replace (1 of 5):

```rust
mod ask;
mod context;
mod doctor;
mod models;
```

with:

```rust
mod ask;
mod auth;
mod context;
mod doctor;
mod models;
```

Replace (2 of 5):

```rust
    },
    /// List models from local servers and configured providers
    Models,
    /// Review the workspace's project settings that widen what the agent may do, and trust them
    Trust {
        /// Trust without asking (for scripts)
```

with:

```rust
    },
    /// List models from local servers and configured providers
    Models,
    /// Store API keys and choose account profiles
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Remove a provider's stored credentials
    Logout {
        /// The provider, e.g. openai
        provider: String,
        /// The account profile (default: the one the provider uses)
        #[arg(long)]
        profile: Option<String>,
    },
    /// Review the workspace's project settings that widen what the agent may do, and trust them
    Trust {
        /// Trust without asking (for scripts)
```

Replace (3 of 5):

```rust
    },
}

#[derive(Subcommand)]
enum SandboxCommand {
    /// Show which sandbox this system gets, how git metadata is protected, and how to improve it
```

with:

```rust
    },
}

#[derive(Subcommand)]
enum AuthCommand {
    /// Store an API key for a provider, read from standard input (typed without echo, or piped)
    Add {
        /// The provider, e.g. openai
        provider: String,
        /// The account profile to store it under
        #[arg(long, default_value = "default")]
        profile: String,
    },
    /// Make a stored account profile the one a provider uses
    Use {
        /// The provider, e.g. openai
        provider: String,
        /// The account profile
        profile: String,
    },
}

#[derive(Subcommand)]
enum SandboxCommand {
    /// Show which sandbox this system gets, how git metadata is protected, and how to improve it
```

Replace (4 of 5):

```rust
    match command {
        Command::Ask { .. } => "harness ask",
        Command::Models => "harness models",
        Command::Trust { .. } => "harness trust",
        Command::Sandbox { .. } => "harness sandbox doctor",
    }
```

with:

```rust
    match command {
        Command::Ask { .. } => "harness ask",
        Command::Models => "harness models",
        Command::Auth {
            command: AuthCommand::Add { .. },
        } => "harness auth add",
        Command::Auth {
            command: AuthCommand::Use { .. },
        } => "harness auth use",
        Command::Logout { .. } => "harness logout",
        Command::Trust { .. } => "harness trust",
        Command::Sandbox { .. } => "harness sandbox doctor",
    }
```

Replace (5 of 5):

```rust
                ask::run(cli.model, cli.mode, session, prompt.join(" "), json).await
            }
            Some(Command::Models) => models::run().await,
            Some(Command::Trust { yes, revoke }) => trust::run(yes, revoke),
            Some(Command::Sandbox {
                command: SandboxCommand::Doctor,
```

with:

```rust
                ask::run(cli.model, cli.mode, session, prompt.join(" "), json).await
            }
            Some(Command::Models) => models::run().await,
            Some(Command::Auth {
                command: AuthCommand::Add { provider, profile },
            }) => auth::add(&provider, &profile),
            Some(Command::Auth {
                command: AuthCommand::Use { provider, profile },
            }) => auth::use_profile(&provider, &profile),
            Some(Command::Logout { provider, profile }) => {
                auth::logout(&provider, profile.as_deref())
            }
            Some(Command::Trust { yes, revoke }) => trust::run(yes, revoke),
            Some(Command::Sandbox {
                command: SandboxCommand::Doctor,
```

In `crates/harness-cli/src/ask.rs`:

Replace:

```rust
        );
        return 2;
    };
    let resolved = match registry::resolve(&model_id, &setup.config.providers, setup::env) {
        Ok(resolved) => resolved,
        Err(e) => {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
```

with:

```rust
        );
        return 2;
    };
    let resolved = match registry::resolve(&model_id, &setup.config.providers, setup.keys()) {
        Ok(resolved) => resolved,
        Err(e) => {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
```

In `crates/harness-cli/src/slash.rs`:

Replace (1 of 2):

```rust
};
use harness_providers::registry;

use crate::{context::home, setup, setup::Setup, term::terminal_safe};

/// The project's custom commands when `prompt` is a slash command, printing a warning for each
/// command file that was ignored. `None` for ordinary prompts.
```

with:

```rust
};
use harness_providers::registry;

use crate::{context::home, setup::Setup, term::terminal_safe};

/// The project's custom commands when `prompt` is a slash command, printing a warning for each
/// command file that was ignored. `None` for ordinary prompts.
```

Replace (2 of 2):

```rust
        input.parts.push(InputPart::Text(piped.to_string()));
    }
    if let Some(model) = expansion.model {
        match registry::resolve(&model, &setup.config.providers, setup::env) {
            Ok(resolved) => {
                eprintln!("{}", runs_on(&command.name, &resolved.id));
                input.model = Some(TurnModel {
```

with:

```rust
        input.parts.push(InputPart::Text(piped.to_string()));
    }
    if let Some(model) = expansion.model {
        match registry::resolve(&model, &setup.config.providers, setup.keys()) {
            Ok(resolved) => {
                eprintln!("{}", runs_on(&command.name, &resolved.id));
                input.model = Some(TurnModel {
```

In `crates/harness-cli/src/models.rs`:

Replace:

```rust
/// Models from local servers and configured providers, local first.
pub async fn available(setup: &Setup) -> Vec<DiscoveredModel> {
    let local = registry::local_endpoints(&setup.config.providers);
    let configured = registry::configured_endpoints(&setup.config.providers, setup::env);
    let (mut found, remote) = tokio::join!(
        discovery::list_models(&local, LOCAL_PROBE_TIMEOUT),
        discovery::list_models(&configured, REMOTE_PROBE_TIMEOUT)
```

with:

```rust
/// Models from local servers and configured providers, local first.
pub async fn available(setup: &Setup) -> Vec<DiscoveredModel> {
    let local = registry::local_endpoints(&setup.config.providers);
    let configured = registry::configured_endpoints(&setup.config.providers, setup.keys());
    let (mut found, remote) = tokio::join!(
        discovery::list_models(&local, LOCAL_PROBE_TIMEOUT),
        discovery::list_models(&configured, REMOTE_PROBE_TIMEOUT)
```

- [ ] **Step 7: Keep every end-to-end test off the keychain**

Each suite's harness invocations get `HARNESS_CREDENTIAL_STORE=file` right after `HARNESS_HOME`. Run:

```bash
python3 - <<'PY'
import glob, re
for path in sorted(glob.glob("crates/harness-cli/tests/*.rs")):
    if path.endswith(("auth_e2e.rs", "cli_smoke.rs")):
        continue
    lines = open(path).read().split("\n")
    out = []
    for i, line in enumerate(lines):
        out.append(line)
        if '.env("HARNESS_HOME"' in line and "HARNESS_CREDENTIAL_STORE" not in lines[i + 1]:
            indent = re.match(r"\s*", line).group(0)
            if line.endswith(";"):
                out[-1] = line[:-1]
                out.append(f'{indent}.env("HARNESS_CREDENTIAL_STORE", "file");')
            else:
                out.append(f'{indent}.env("HARNESS_CREDENTIAL_STORE", "file")')
    open(path, "w").write("\n".join(out))
PY
grep -c HARNESS_CREDENTIAL_STORE crates/harness-cli/tests/*.rs
```

Expected: `ask_e2e.rs:7`, `checkpoints_e2e.rs:2`, `commands_e2e.rs:1`, `compaction_e2e.rs:1`, `context_e2e.rs:1`, `init_e2e.rs:1`, `sandbox_e2e.rs:2`, `sessions_e2e.rs:3`, `trust_e2e.rs:1` (with `auth_e2e.rs` and `cli_smoke.rs` from Step 1). Every `.env("HARNESS_HOME", …)` in those files is followed by the new line.

- [ ] **Step 8: Run the tests**

Run: `cargo test -p harness-providers -p harness-cli`
Expected: PASS: 10 in `credentials` (among them `a_readable_credentials_file_is_made_private_again`), 15 in `registry`, 4 in `auth_e2e` and 8 in `cli_smoke`; 206 tests in the two crates. Run in the foreground: `ask_e2e`'s `ctrl_c_during_a_never_closing_stdin_exits_130` fails when the tests run as a background job, at `f2cc9fb` too (see "How this plan was checked").

- [ ] **Step 9: Lint and check the new crates**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo deny check`
Expected: clean; `cargo deny` reports `advisories ok, bans ok, licenses ok, sources ok`, with the same duplicate-version warnings as before.

- [ ] **Step 10: Commit**

```bash
git add Cargo.toml Cargo.lock crates/harness-providers crates/harness-cli
git commit -F - <<'EOF'
feat(providers): store API keys and choose account profiles

harness auth add reads a key from standard input (without echo on a
terminal) and stores it in the OS keychain through keyring-core, or in
credentials.json (mode 0600) in the data directory, with a warning,
when no keychain can be used. harness auth use picks a provider's
account profile and harness logout removes one. A provider with a key
variable uses it first, then the stored key; one without takes no key,
so no request to a local server reads the keychain. A Claude
subscription token is refused wherever it comes from, and nothing
under ~/.claude is read. The end-to-end tests use the file store.

<trailer lines from the controller>
EOF
```

---

### Task 5: ChatGPT sign-in flows

**Files:**
- Create: `crates/harness-providers/src/chatgpt/mod.rs`, `crates/harness-providers/src/chatgpt/oauth.rs`, `crates/harness-providers/tests/chatgpt_oauth.rs`
- Modify: `Cargo.toml`, `Cargo.lock` (cargo updates it), `crates/harness-providers/Cargo.toml`, `crates/harness-providers/src/lib.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces, behind the default-on feature `chatgpt-login` of `harness-providers`, in `harness_providers::chatgpt::oauth`:
  - `ISSUER = "https://auth.openai.com"`, `CLIENT_ID = "app_EMoamEEZ73f0CkXaXp7hrann"`, `SCOPES = "openid profile email offline_access api.connectors.read api.connectors.invoke"`, `CALLBACK_PORTS = [1455, 1457]`, `ORIGINATOR = "codex_cli_rs"` (decision 2: Codex's own originator, not a `harness`-specific one), `DEVICE_CODE_WAIT` (15 minutes);
  - `OAuthError { Network, Rejected { status, body }, Invalid, Denied, TimedOut, Io }`; `Rejected` says to run `harness login chatgpt`;
  - `Pkce { verifier, challenge }` with `Pkce::generate()` and `Pkce::from_verifier(&str)`, and `random_state()`, from `/dev/urandom`;
  - `Tokens { access_token, refresh_token, account_id: Option<String>, email: Option<String> }` with `expires_at() -> Option<u64>` (the access token's `exp`), `to_json()`, `from_json(&str) -> Option<Tokens>`, and a `Debug` without the tokens;
  - `DeviceCode { verification_url, user_code, .. }`;
  - `OAuth::new(issuer)` with `authorize_url(redirect_uri, &Pkce, state) -> String`, `exchange_code(code, redirect_uri, verifier) -> Result<Tokens, _>`, `request_device_code() -> Result<DeviceCode, _>`, `poll_device_code(&DeviceCode, max_wait) -> Result<Tokens, _>`, `refresh(&Tokens) -> Result<Tokens, _>`;
  - `CallbackServer::bind(ports: &[u16])` (`0` for any port), `port()`, `redirect_uri()` and `wait_for_code(state) -> Result<String, OAuthError>`.

These are Codex's flows (decision 1), written from scratch: the callback server is a few dozen lines of `tokio` rather than an HTTP server crate, it listens on `127.0.0.1` only, answers in plain text so nothing from the query is ever rendered as HTML, drops a client that sends nothing for five seconds, and ignores a request with another `state`. A form body is built with `reqwest`'s `Url`, which avoids a new `reqwest` feature.

- [ ] **Step 1: Declare the feature and write the failing tests**

In `Cargo.toml`:

Replace:

```toml
harness-shell = { path = "crates/harness-shell" }
harness-tools = { path = "crates/harness-tools" }
async-stream = "0.3"
async-trait = "0.1.92"
clap = { version = "4.6.7", features = ["derive"] }
eventsource-stream = "0.2.3"
```

with:

```toml
harness-shell = { path = "crates/harness-shell" }
harness-tools = { path = "crates/harness-tools" }
async-stream = "0.3"
base64 = "0.23.1"
async-trait = "0.1.92"
clap = { version = "4.6.7", features = ["derive"] }
eventsource-stream = "0.2.3"
```

In `crates/harness-providers/Cargo.toml`, the feature (its `sha2` comes in Step 3):

Replace:

```toml
license.workspace = true
publish.workspace = true

[dependencies]
async-stream.workspace = true
eventsource-stream.workspace = true
futures.workspace = true
harness-config.workspace = true
```

with:

```toml
license.workspace = true
publish.workspace = true

[features]
default = ["chatgpt-login"]
# ChatGPT sign-in, isolated so that it can be left out of a build quickly if OpenAI's policy on
# third-party use changes.
chatgpt-login = ["dep:base64"]

[dependencies]
async-stream.workspace = true
base64 = { workspace = true, optional = true }
eventsource-stream.workspace = true
futures.workspace = true
harness-config.workspace = true
```

Create `crates/harness-providers/tests/chatgpt_oauth.rs`:

```rust
//! ChatGPT sign-in against a mock OAuth server: PKCE, the browser flow's localhost callback, the
//! device-code flow, and token refresh. Nothing here talks to OpenAI.
#![cfg(feature = "chatgpt-login")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use harness_providers::chatgpt::oauth::{
    CLIENT_ID, CallbackServer, OAuth, OAuthError, Pkce, Tokens,
};
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn jwt(claims: Value) -> String {
    let part = |value: &Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap());
    format!(
        "{}.{}.{}",
        part(&json!({"alg": "none"})),
        part(&claims),
        URL_SAFE_NO_PAD.encode(b"sig")
    )
}

/// What the auth server answers a code exchange with.
fn token_response() -> Value {
    json!({
        "id_token": jwt(json!({
            "email": "dev@example.com",
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-123", "chatgpt_plan_type": "plus"}
        })),
        "access_token": jwt(json!({"exp": 4_102_444_800u64})),
        "refresh_token": "rt-1",
    })
}

// RFC 7636, appendix B.
#[test]
fn the_pkce_challenge_is_the_verifiers_sha256_in_base64url() {
    let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
    assert_eq!(
        pkce.challenge,
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
    let fresh = Pkce::generate().unwrap();
    assert_eq!(fresh.verifier.len(), 86);
    assert!(
        fresh
            .verifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    );
    assert_ne!(fresh.verifier, Pkce::generate().unwrap().verifier);
}

#[test]
fn the_authorize_url_asks_for_a_code_with_pkce() {
    let oauth = OAuth::new("https://auth.example");
    let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
    let url = reqwest::Url::parse(&oauth.authorize_url(
        "http://127.0.0.1:1455/auth/callback",
        &pkce,
        "st",
    ))
    .unwrap();
    assert_eq!(
        url.as_str().split('?').next(),
        Some("https://auth.example/oauth/authorize")
    );
    let query: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
    for (key, value) in [
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", "http://127.0.0.1:1455/auth/callback"),
        (
            "scope",
            "openid profile email offline_access api.connectors.read api.connectors.invoke",
        ),
        (
            "code_challenge",
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
        ),
        ("code_challenge_method", "S256"),
        ("state", "st"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", "codex_cli_rs"),
    ] {
        assert_eq!(query.get(key).map(String::as_str), Some(value), "{key}");
    }
}

/// Plays the browser: follows the redirect back to the callback server.
async fn browser_returns(redirect_uri: &str, query: &str) -> (u16, String) {
    let response = reqwest::get(format!("{redirect_uri}?{query}"))
        .await
        .unwrap();
    (response.status().as_u16(), response.text().await.unwrap())
}

#[tokio::test]
async fn the_browser_flow_exchanges_the_code_from_the_callback() {
    let server = MockServer::start().await;
    let callback = CallbackServer::bind(&[0]).await.unwrap();
    let redirect_uri = callback.redirect_uri();
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code=the-code"))
        .and(body_string_contains(
            "code_verifier=dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
        ))
        .and(body_string_contains(
            format!("client_id={CLIENT_ID}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_response()))
        .expect(1)
        .mount(&server)
        .await;
    let browser = tokio::spawn({
        let redirect_uri = redirect_uri.clone();
        async move { browser_returns(&redirect_uri, "code=the-code&state=st-1").await }
    });
    let code = callback.wait_for_code("st-1").await.unwrap();
    assert_eq!(code, "the-code");
    let (status, page) = browser.await.unwrap();
    assert_eq!(status, 200);
    assert!(page.contains("signed in"), "{page}");
    let oauth = OAuth::new(&server.uri());
    let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
    let tokens = oauth
        .exchange_code(&code, &redirect_uri, &pkce.verifier)
        .await
        .unwrap();
    assert_eq!(tokens.account_id.as_deref(), Some("acct-123"));
    assert_eq!(tokens.email.as_deref(), Some("dev@example.com"));
    assert_eq!(tokens.refresh_token, "rt-1");
    assert_eq!(tokens.expires_at(), Some(4_102_444_800));
}

// Review Focus: a callback with another state (another tab, or a forged link) is refused, and the
// server keeps waiting for the real one.
#[tokio::test]
async fn a_callback_with_the_wrong_state_is_refused() {
    let callback = CallbackServer::bind(&[0]).await.unwrap();
    let redirect_uri = callback.redirect_uri();
    let browser = tokio::spawn(async move {
        let wrong = browser_returns(&redirect_uri, "code=forged&state=other").await;
        let lost = reqwest::get(redirect_uri.replace("/auth/callback", "/favicon.ico"))
            .await
            .unwrap()
            .status()
            .as_u16();
        let right = browser_returns(&redirect_uri, "code=real&state=st-2").await;
        (wrong, lost, right)
    });
    assert_eq!(callback.wait_for_code("st-2").await.unwrap(), "real");
    let ((status, _), lost, (right, _)) = browser.await.unwrap();
    assert_eq!(status, 400);
    assert_eq!(lost, 404);
    assert_eq!(right, 200);
}

#[tokio::test]
async fn a_refused_sign_in_ends_the_wait_with_the_reason() {
    let callback = CallbackServer::bind(&[0]).await.unwrap();
    let redirect_uri = callback.redirect_uri();
    let browser = tokio::spawn(async move {
        browser_returns(
            &redirect_uri,
            "error=access_denied&error_description=The+user+declined&state=st-3",
        )
        .await
    });
    let error = callback.wait_for_code("st-3").await.unwrap_err();
    assert!(
        matches!(&error, OAuthError::Denied(reason) if reason.contains("The user declined")),
        "{error:?}"
    );
    let (_, page) = browser.await.unwrap();
    // A plain-text page: nothing from the query is rendered as HTML.
    assert!(page.contains("The user declined"), "{page}");
}

#[tokio::test]
async fn the_callback_moves_to_the_next_port_when_one_is_taken() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let callback = CallbackServer::bind(&[port, 0]).await.unwrap();
    assert_ne!(callback.port(), port);
    assert!(callback.redirect_uri().starts_with("http://127.0.0.1:"));
    assert!(callback.redirect_uri().ends_with("/auth/callback"));
}

async fn mock_usercode(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/usercode"))
        .and(body_string_contains(CLIENT_ID))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_auth_id": "device-auth-123",
            "user_code": "CODE-12345",
            "interval": "0"
        })))
        .mount(server)
        .await;
}

// Spec: "Sign-in over SSH".
#[tokio::test]
async fn the_device_flow_polls_until_the_user_approves() {
    let server = MockServer::start().await;
    mock_usercode(&server).await;
    let polls = Arc::new(AtomicUsize::new(0));
    let counter = polls.clone();
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/token"))
        .and(body_string_contains("device-auth-123"))
        .respond_with(move |_: &Request| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(403)
            } else {
                ResponseTemplate::new(200).set_body_json(json!({
                    "authorization_code": "poll-code",
                    "code_challenge": "challenge",
                    "code_verifier": "poll-verifier"
                }))
            }
        })
        .mount(&server)
        .await;
    let callback = format!("{}/deviceauth/callback", server.uri());
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("code=poll-code"))
        .and(body_string_contains("code_verifier=poll-verifier"))
        .and(body_string_contains(
            format!("redirect_uri={}", urlencode(&callback)).as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_response()))
        .mount(&server)
        .await;
    let oauth = OAuth::new(&server.uri());
    let device = oauth.request_device_code().await.unwrap();
    assert_eq!(device.user_code, "CODE-12345");
    assert_eq!(
        device.verification_url,
        format!("{}/codex/device", server.uri())
    );
    let tokens = oauth
        .poll_device_code(&device, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(tokens.account_id.as_deref(), Some("acct-123"));
    assert_eq!(polls.load(Ordering::SeqCst), 2);
}

fn urlencode(text: &str) -> String {
    text.replace(':', "%3A").replace('/', "%2F")
}

#[tokio::test]
async fn the_device_flow_gives_up_after_its_time_limit() {
    let server = MockServer::start().await;
    mock_usercode(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/token"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let oauth = OAuth::new(&server.uri());
    let device = oauth.request_device_code().await.unwrap();
    let error = oauth
        .poll_device_code(&device, Duration::from_millis(200))
        .await
        .unwrap_err();
    assert!(matches!(error, OAuthError::TimedOut), "{error:?}");
}

#[tokio::test]
async fn a_refresh_keeps_what_the_server_did_not_replace() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains(r#""grant_type":"refresh_token""#))
        .and(body_string_contains(r#""refresh_token":"rt-1""#))
        .and(body_string_contains(
            format!(r#""client_id":"{CLIENT_ID}""#).as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": jwt(json!({"exp": 4_102_444_900u64})),
        })))
        .mount(&server)
        .await;
    let oauth = OAuth::new(&server.uri());
    let before = Tokens {
        access_token: "old".into(),
        refresh_token: "rt-1".into(),
        account_id: Some("acct-123".into()),
        email: Some("dev@example.com".into()),
    };
    let after = oauth.refresh(&before).await.unwrap();
    assert_eq!(after.expires_at(), Some(4_102_444_900));
    assert_eq!(after.refresh_token, "rt-1");
    assert_eq!(after.account_id.as_deref(), Some("acct-123"));
}

#[tokio::test]
async fn a_rejected_refresh_says_to_sign_in_again() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({"error": {"code": "refresh_token_reused"}})),
        )
        .mount(&server)
        .await;
    let oauth = OAuth::new(&server.uri());
    let tokens = Tokens {
        access_token: "old".into(),
        refresh_token: "rt-used".into(),
        account_id: None,
        email: None,
    };
    let error = oauth.refresh(&tokens).await.unwrap_err();
    assert!(
        matches!(error, OAuthError::Rejected { status: 400, .. }),
        "{error:?}"
    );
    assert!(
        error.to_string().contains("harness login chatgpt"),
        "{error}"
    );
}

#[test]
fn tokens_round_trip_through_storage_and_never_print() {
    let tokens = Tokens {
        access_token: "at-secret".into(),
        refresh_token: "rt-secret".into(),
        account_id: Some("acct-123".into()),
        email: Some("dev@example.com".into()),
    };
    let stored = tokens.to_json();
    assert_eq!(Tokens::from_json(&stored).unwrap(), tokens);
    let shown = format!("{tokens:?}");
    assert!(!shown.contains("secret"), "{shown}");
    assert!(Tokens::from_json("{}").is_none());
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p harness-providers --test chatgpt_oauth`
Expected: FAIL to compile: ``cannot find `chatgpt` in `harness_providers` ``.

- [ ] **Step 3: Implement the flows**

In `crates/harness-providers/Cargo.toml`, the feature also needs `sha2`:

Replace (1 of 2):

```toml
default = ["chatgpt-login"]
# ChatGPT sign-in, isolated so that it can be left out of a build quickly if OpenAI's policy on
# third-party use changes.
chatgpt-login = ["dep:base64"]

[dependencies]
async-stream.workspace = true
```

with:

```toml
default = ["chatgpt-login"]
# ChatGPT sign-in, isolated so that it can be left out of a build quickly if OpenAI's policy on
# third-party use changes.
chatgpt-login = ["dep:base64", "dep:sha2"]

[dependencies]
async-stream.workspace = true
```

Replace (2 of 2):

```toml
reqwest.workspace = true
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
tokio.workspace = true
toml.workspace = true
```

with:

```toml
reqwest.workspace = true
serde.workspace = true
serde_json.workspace = true
sha2 = { workspace = true, optional = true }
thiserror.workspace = true
tokio.workspace = true
toml.workspace = true
```

Create `crates/harness-providers/src/chatgpt/mod.rs`:

```rust
//! ChatGPT sign-in and the `chatgpt` provider (feature `chatgpt-login`). The OAuth flows follow
//! OpenAI's open-source Codex CLI (github.com/openai/codex, Apache-2.0, `codex-rs/login`), use
//! its public client, and identify to OpenAI as it (the Codex CLI's `originator` and scopes, not
//! a `harness`-specific identity). This is not an OpenAI endorsement of harness.

pub mod oauth;
```

Create `crates/harness-providers/src/chatgpt/oauth.rs`:

```rust
//! ChatGPT sign-in: OAuth 2.0 authorization code with PKCE, through the browser and a callback
//! on `127.0.0.1`, or through a device code; and refreshing the access token. The endpoints,
//! client id, scopes, originator and callback ports are those of OpenAI's Codex CLI (openai/codex,
//! `codex-rs/login/src/server.rs`, `device_code_auth.rs` and `auth/manager.rs`): harness signs in
//! with the Codex CLI's OAuth client and identifies to OpenAI as it, not as a distinct client.

use std::{
    collections::HashMap,
    io::Read,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

/// OpenAI's authorization server.
pub const ISSUER: &str = "https://auth.openai.com";
/// The public OAuth client of OpenAI's Codex CLI.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// What harness asks for: who the user is, a refresh token, and the connector scopes the Codex
/// CLI also asks for (unused by harness, but part of identifying as it).
pub const SCOPES: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";
/// The callback ports registered for that client: the first, then the fallback.
pub const CALLBACK_PORTS: [u16; 2] = [1455, 1457];
/// How harness names itself to the authorization server and to ChatGPT's backend: the Codex
/// CLI's own originator, not harness's. harness signs in with the Codex CLI's OAuth client and
/// identifies to OpenAI as it, rather than presenting itself as a distinct client.
pub const ORIGINATOR: &str = "codex_cli_rs";
/// How long a device code stays valid.
pub const DEVICE_CODE_WAIT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("cannot reach the sign-in server: {0}")]
    Network(String),
    #[error(
        "the sign-in server refused (HTTP {status}): {body}; run `harness login chatgpt` to sign in again"
    )]
    Rejected { status: u16, body: String },
    #[error("the sign-in server answered something unexpected: {0}")]
    Invalid(String),
    #[error("sign-in was refused: {0}")]
    Denied(String),
    #[error("sign-in timed out")]
    TimedOut,
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// A PKCE verifier and its S256 challenge.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    /// A fresh verifier: 64 random bytes in base64url.
    pub fn generate() -> std::io::Result<Pkce> {
        Ok(Pkce::from_verifier(
            &URL_SAFE_NO_PAD.encode(random_bytes::<64>()?),
        ))
    }

    pub fn from_verifier(verifier: &str) -> Pkce {
        Pkce {
            verifier: verifier.to_string(),
            challenge: URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        }
    }
}

/// A random `state` for one sign-in.
pub fn random_state() -> std::io::Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(random_bytes::<32>()?))
}

fn random_bytes<const N: usize>() -> std::io::Result<[u8; N]> {
    let mut bytes = [0u8; N];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// What a sign-in leaves: the tokens, and the ChatGPT account and email from the ID token.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Sent as `ChatGPT-Account-ID` with every request.
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

/// Leaves the tokens out.
impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens")
            .field("access_token", &"[redacted]")
            .field("refresh_token", &"[redacted]")
            .field("account_id", &self.account_id)
            .field("email", &self.email)
            .finish()
    }
}

impl Tokens {
    /// When the access token expires, in seconds since the Unix epoch, from its `exp` claim.
    pub fn expires_at(&self) -> Option<u64> {
        claims(&self.access_token)?["exp"].as_u64()
    }

    /// How the tokens are stored.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("tokens serialize")
    }

    /// Stored tokens, or `None` for something that is not.
    pub fn from_json(text: &str) -> Option<Tokens> {
        serde_json::from_str(text).ok()
    }

    /// Takes the account and email from `id_token`, when there is one.
    fn with_identity(mut self, id_token: Option<&str>) -> Tokens {
        if let Some(claims) = id_token.and_then(claims) {
            if let Some(account) =
                claims["https://api.openai.com/auth"]["chatgpt_account_id"].as_str()
            {
                self.account_id = Some(account.to_string());
            }
            let email = claims["email"]
                .as_str()
                .or_else(|| claims["https://api.openai.com/profile"]["email"].as_str());
            if let Some(email) = email {
                self.email = Some(email.to_string());
            }
        }
        self
    }
}

/// The claims of a JWT, without checking its signature: harness only reads what the server it
/// just talked to sent.
fn claims(jwt: &str) -> Option<Value> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// A device code the user enters at `verification_url`.
#[derive(Debug, Clone)]
pub struct DeviceCode {
    pub verification_url: String,
    pub user_code: String,
    device_auth_id: String,
    interval: Duration,
}

/// The authorization server's endpoints, for one client.
pub struct OAuth {
    client: reqwest::Client,
    issuer: String,
    client_id: String,
}

impl OAuth {
    pub fn new(issuer: &str) -> OAuth {
        OAuth {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent(concat!("harness/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("an HTTP client builds"),
            issuer: issuer.trim_end_matches('/').to_string(),
            client_id: CLIENT_ID.to_string(),
        }
    }

    /// Where the browser goes to sign in.
    pub fn authorize_url(&self, redirect_uri: &str, pkce: &Pkce, state: &str) -> String {
        let mut url = reqwest::Url::parse(&format!("{}/oauth/authorize", self.issuer))
            .expect("the issuer is a URL");
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", SCOPES)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state)
            .append_pair("id_token_add_organizations", "true")
            .append_pair("codex_cli_simplified_flow", "true")
            .append_pair("originator", ORIGINATOR);
        url.to_string()
    }

    /// Exchanges an authorization code for tokens.
    pub async fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> Result<Tokens, OAuthError> {
        let body = form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", &self.client_id),
            ("code_verifier", verifier),
        ]);
        let response = self
            .client
            .post(format!("{}/oauth/token", self.issuer))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await;
        let value = read(response).await?;
        let tokens = Tokens {
            access_token: string(&value, "access_token")?,
            refresh_token: string(&value, "refresh_token")?,
            account_id: None,
            email: None,
        };
        Ok(tokens.with_identity(value["id_token"].as_str()))
    }

    /// Asks for a device code.
    pub async fn request_device_code(&self) -> Result<DeviceCode, OAuthError> {
        let response = self
            .client
            .post(format!("{}/api/accounts/deviceauth/usercode", self.issuer))
            .json(&json!({"client_id": self.client_id}))
            .send()
            .await;
        let value = read(response).await?;
        let user_code = value["user_code"]
            .as_str()
            .or_else(|| value["usercode"].as_str())
            .ok_or_else(|| OAuthError::Invalid("no user code".into()))?;
        // Sent as a string, "5"; a number is taken too.
        let interval = value["interval"]
            .as_u64()
            .or_else(|| value["interval"].as_str()?.trim().parse().ok())
            .unwrap_or(5);
        Ok(DeviceCode {
            verification_url: format!("{}/codex/device", self.issuer),
            user_code: user_code.to_string(),
            device_auth_id: string(&value, "device_auth_id")?,
            interval: Duration::from_secs(interval),
        })
    }

    /// Waits, up to `max_wait`, for the user to enter the device code, then exchanges the code
    /// the server issues for tokens.
    pub async fn poll_device_code(
        &self,
        device: &DeviceCode,
        max_wait: Duration,
    ) -> Result<Tokens, OAuthError> {
        let started = Instant::now();
        loop {
            let response = self
                .client
                .post(format!("{}/api/accounts/deviceauth/token", self.issuer))
                .json(&json!({"device_auth_id": device.device_auth_id, "user_code": device.user_code}))
                .send()
                .await
                .map_err(|e| OAuthError::Network(e.to_string()))?;
            // Not approved yet.
            if matches!(response.status().as_u16(), 403 | 404) {
                let waited = started.elapsed();
                if waited >= max_wait {
                    return Err(OAuthError::TimedOut);
                }
                let pause = device.interval.max(Duration::from_millis(100));
                tokio::time::sleep(pause.min(max_wait - waited)).await;
                continue;
            }
            let value = read(Ok(response)).await?;
            let redirect_uri = format!("{}/deviceauth/callback", self.issuer);
            return self
                .exchange_code(
                    &string(&value, "authorization_code")?,
                    &redirect_uri,
                    &string(&value, "code_verifier")?,
                )
                .await;
        }
    }

    /// New tokens for `tokens`. What the server does not replace (the refresh token, often) is
    /// kept.
    pub async fn refresh(&self, tokens: &Tokens) -> Result<Tokens, OAuthError> {
        let response = self
            .client
            .post(format!("{}/oauth/token", self.issuer))
            .json(&json!({
                "client_id": self.client_id,
                "grant_type": "refresh_token",
                "refresh_token": tokens.refresh_token,
            }))
            .send()
            .await;
        let value = read(response).await?;
        let refreshed = Tokens {
            access_token: string(&value, "access_token")?,
            refresh_token: value["refresh_token"]
                .as_str()
                .unwrap_or(&tokens.refresh_token)
                .to_string(),
            ..tokens.clone()
        };
        Ok(refreshed.with_identity(value["id_token"].as_str()))
    }
}

/// `pairs` as an `application/x-www-form-urlencoded` body.
fn form(pairs: &[(&str, &str)]) -> String {
    let mut url = reqwest::Url::parse("http://form.invalid/").expect("a static URL");
    url.query_pairs_mut().extend_pairs(pairs);
    url.query().unwrap_or_default().to_string()
}

/// The JSON of a successful response.
async fn read(response: reqwest::Result<reqwest::Response>) -> Result<Value, OAuthError> {
    let response = response.map_err(|e| OAuthError::Network(e.to_string()))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| OAuthError::Network(e.to_string()))?;
    if !status.is_success() {
        return Err(OAuthError::Rejected {
            status: status.as_u16(),
            body: text.chars().take(500).collect(),
        });
    }
    serde_json::from_str(&text).map_err(|e| OAuthError::Invalid(e.to_string()))
}

fn string(value: &Value, key: &str) -> Result<String, OAuthError> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from)
        .ok_or_else(|| OAuthError::Invalid(format!("no {key}")))
}

/// The browser's way back: a small HTTP server on `127.0.0.1` that waits for the redirect from
/// the authorization server.
pub struct CallbackServer {
    listener: TcpListener,
    port: u16,
}

impl CallbackServer {
    /// Listens at the first of `ports` that is free (`0`: any).
    pub async fn bind(ports: &[u16]) -> std::io::Result<CallbackServer> {
        let mut last = None;
        for &port in ports {
            match TcpListener::bind(("127.0.0.1", port)).await {
                Ok(listener) => {
                    let port = listener.local_addr()?.port();
                    return Ok(CallbackServer { listener, port });
                }
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| std::io::Error::other("no port to listen on")))
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The redirect URI the authorization server sends the browser back to.
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}/auth/callback", self.port)
    }

    /// Waits for the redirect that carries `state`, and returns its code. A request with another
    /// state, or for another path, is answered and ignored.
    pub async fn wait_for_code(&self, state: &str) -> Result<String, OAuthError> {
        loop {
            let (mut stream, _) = self.listener.accept().await?;
            let Some(target) = request_target(&mut stream).await else {
                respond(&mut stream, 400, "Bad request.").await;
                continue;
            };
            let url = match reqwest::Url::parse(&format!("http://127.0.0.1{target}")) {
                Ok(url) if url.path() == "/auth/callback" => url,
                _ => {
                    respond(&mut stream, 404, "Not found.").await;
                    continue;
                }
            };
            let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
            if query.get("state").map(String::as_str) != Some(state) {
                respond(
                    &mut stream,
                    400,
                    "This sign-in is not the one harness started. Go back to the terminal.",
                )
                .await;
                continue;
            }
            if let Some(error) = query.get("error") {
                let reason = query
                    .get("error_description")
                    .filter(|d| !d.is_empty())
                    .unwrap_or(error)
                    .clone();
                let page = format!("Sign-in failed: {reason}. Go back to the terminal.");
                respond(&mut stream, 200, &page).await;
                return Err(OAuthError::Denied(reason));
            }
            let Some(code) = query.get("code").filter(|c| !c.is_empty()) else {
                respond(&mut stream, 400, "The sign-in carried no code.").await;
                return Err(OAuthError::Invalid("the callback carried no code".into()));
            };
            respond(
                &mut stream,
                200,
                "harness is signed in to ChatGPT. You can close this tab.",
            )
            .await;
            return Ok(code.clone());
        }
    }
}

/// The target of an HTTP `GET` request, from its request line. A client that sends nothing for
/// five seconds is dropped, so it cannot hold up the real callback.
async fn request_target(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    let read = async {
        while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 16 * 1024 {
            let n = stream.read(&mut buf).await.ok()?;
            if n == 0 {
                break;
            }
            head.extend_from_slice(&buf[..n]);
        }
        Some(())
    };
    tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .ok()??;
    let head = String::from_utf8_lossy(&head);
    let mut words = head.lines().next()?.split_whitespace();
    match (words.next(), words.next()) {
        (Some("GET"), Some(target)) if target.starts_with('/') => Some(target.to_string()),
        _ => None,
    }
}

/// A plain-text page: nothing from the request is ever rendered as HTML.
async fn respond(stream: &mut TcpStream, status: u16, text: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}
```

In `crates/harness-providers/src/lib.rs`:

Replace:

```rust
//! Model provider adapters, local-server discovery, and model-id resolution.

pub mod anthropic_messages;
pub mod credentials;
pub mod discovery;
pub mod openai_chat;
```

with:

```rust
//! Model provider adapters, local-server discovery, and model-id resolution.

pub mod anthropic_messages;
#[cfg(feature = "chatgpt-login")]
pub mod chatgpt;
pub mod credentials;
pub mod discovery;
pub mod openai_chat;
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p harness-providers`
Expected: PASS: 11 in `chatgpt_oauth` (among them `a_callback_with_the_wrong_state_is_refused`); 85 tests in the crate.

- [ ] **Step 5: Lint, with and without the feature**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo clippy -p harness-providers --no-default-features --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/harness-providers
git commit -F - <<'EOF'
feat(providers): sign in to ChatGPT with OAuth and PKCE

The chatgpt-login feature (on by default) adds the sign-in flows of
OpenAI's Codex CLI with its public client: an authorize URL with an
S256 PKCE challenge, a callback server on 127.0.0.1 (port 1455, else
1457) that ignores requests with another state and answers in plain
text, the code exchange, the device-code flow with its 15-minute
limit, and refreshing tokens, keeping what the server does not
replace. Tokens keep the ChatGPT account id and email from the ID
token and never print.

<trailer lines from the controller>
EOF
```

---

### Task 6: `harness login`, `chatgpt/*` models, and exhausted quotas

**Files:**
- Create: `crates/harness-providers/src/chatgpt/auth.rs`, `crates/harness-providers/tests/chatgpt_provider.rs`, `crates/harness-cli/src/login.rs`, `crates/harness-cli/tests/login_e2e.rs`
- Modify: `Cargo.toml`, `Cargo.lock` (cargo updates it), `crates/harness-core/src/provider.rs`, `crates/harness-core/src/agent.rs`, `crates/harness-core/tests/resilience.rs`, `crates/harness-providers/src/chatgpt/mod.rs`, `crates/harness-providers/src/openai_responses.rs`, `crates/harness-providers/src/registry.rs`, `crates/harness-cli/Cargo.toml`, `crates/harness-cli/src/main.rs`, `crates/harness-cli/src/setup.rs`

**Interfaces:**
- Consumes: `chatgpt::oauth` (Task 5), `Credentials` and `Secrets` (Task 4), `OpenAiResponses` (Task 2).
- Produces:
  - `ProviderError::is_quota_exhausted(&self) -> bool` and `ProviderError::resets_at(&self) -> Option<u64>`; `is_retryable` is false for an exhausted quota, and the agent's error message names the reset time and `--model`.
  - `harness_providers::chatgpt::auth::{PROVIDER = "chatgpt", BASE_URL = "https://chatgpt.com/backend-api/codex", REFRESH_MARGIN (5 minutes), ChatGptAuth}` with `ChatGptAuth::load(Arc<Credentials>, profile, OAuth) -> Result<Option<ChatGptAuth>, CredentialError>`, `current() -> Result<Tokens, ProviderError>` (refreshed first within the margin) and `after_unauthorized(used: &str) -> Result<Tokens, ProviderError>`.
  - `OpenAiResponses::chatgpt(base_url, Arc<ChatGptAuth>)`: sends the account header and `originator`, no output limit, and after a 401 renews the tokens and sends the request once more.
  - `registry::CHATGPT`, a `chatgpt` entry in `BUILTIN_PROVIDERS` (`[Builtin; 7]`), `Secrets::credentials(&self) -> Option<Arc<Credentials>>` (default `None`), and `ResolveError::{NotSignedIn { provider, profile }, SignInUnavailable}`. `HARNESS_CHATGPT_ISSUER` and `HARNESS_CHATGPT_BASE_URL` point sign-in and requests at a mock server, for tests.
  - `harness-cli`'s own `chatgpt-login` feature (default on) enabling `harness-providers/chatgpt-login`; the workspace takes `harness-providers` without default features, so `--no-default-features` on `harness-cli` leaves sign-in out. `Setup::credentials` becomes `Arc<Credentials>`. `login::{NOTICE, run, wants_device_flow}`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/harness-core/tests/resilience.rs`, after a blank line:

```rust
fn quota(body: &str) -> ProviderError {
    ProviderError::Http {
        status: 429,
        body: body.to_string(),
        retry_after: None,
    }
}

const USAGE_LIMIT: &str = r#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","plan_type":"plus","resets_at":1790208000}}"#;

#[test]
fn exhausted_quotas_are_not_worth_retrying() {
    for body in [
        USAGE_LIMIT,
        r#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details.","type":"insufficient_quota","code":"insufficient_quota"}}"#,
        r#"{"error":{"type":"usage_not_included","message":"Upgrade to use this model"}}"#,
    ] {
        let error = quota(body);
        assert!(error.is_quota_exhausted(), "{body}");
        assert!(!error.is_retryable(), "{body}");
    }
    let busy = quota(r#"{"error":{"type":"rate_limit_exceeded","message":"Slow down"}}"#);
    assert!(!busy.is_quota_exhausted());
    assert!(busy.is_retryable());
    assert_eq!(quota(USAGE_LIMIT).resets_at(), Some(1_790_208_000));
    let relative = quota(r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":60}}"#);
    let resets = relative.resets_at().unwrap();
    let now = harness_core::time::now_unix();
    assert!((now + 55..=now + 65).contains(&resets), "{resets} vs {now}");
}

// Spec: "Subscription limit reached".
#[tokio::test(start_paused = true)]
async fn a_usage_limit_ends_the_turn_with_its_reset_time() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::error(quota(USAGE_LIMIT)),
        Script::text("later"),
    ]);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Error);
    assert!(retries(&events).is_empty());
    assert_eq!(provider.requests().len(), 1);
    let message = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Error { message, .. } => Some(message.clone()),
            _ => None,
        })
        .unwrap();
    assert!(message.contains("2026-09-24T00:00:00Z"), "{message}");
    assert!(message.contains("--model"), "{message}");
    // The session stays usable.
    let (reason, _) = run(&mut agent, "again").await;
    assert_eq!(reason, TurnEndReason::Completed);
}
```

Create `crates/harness-providers/tests/chatgpt_provider.rs`:

```rust
//! The `chatgpt` provider: the Responses protocol to ChatGPT's backend with a signed-in
//! account, whose tokens are refreshed before they expire and after a 401. A mock server plays
//! both the backend and the authorization server; tokens live in a file store in a temporary
//! directory.
#![cfg(feature = "chatgpt-login")]

use std::collections::BTreeMap;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::StreamExt;
use harness_config::config::Protocol;
use harness_core::message::{ChatRequest, Message, RequestOptions};
use harness_core::provider::{Provider, ProviderError, ProviderEvent};
use harness_providers::chatgpt::auth::ChatGptAuth;
use harness_providers::chatgpt::oauth::{OAuth, Tokens};
use harness_providers::credentials::Credentials;
use harness_providers::openai_responses::OpenAiResponses;
use harness_providers::registry::{ResolveError, Secrets, resolve};
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn jwt(claims: Value) -> String {
    let part = |value: &Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap());
    format!(
        "{}.{}.{}",
        part(&json!({"alg": "none"})),
        part(&claims),
        URL_SAFE_NO_PAD.encode(b"sig")
    )
}

/// An access token that expires `secs` from now.
fn access_token(label: &str, secs: u64) -> String {
    jwt(json!({"exp": harness_core::time::now_unix() + secs, "label": label}))
}

fn tokens(access: &str, refresh: &str) -> Tokens {
    Tokens {
        access_token: access.to_string(),
        refresh_token: refresh.to_string(),
        account_id: Some("acct-123".into()),
        email: Some("dev@example.com".into()),
    }
}

fn text_reply() -> ResponseTemplate {
    let body = [
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": "hi from chatgpt"}),
        json!({"type": "response.completed", "response": {"status": "completed"}}),
    ]
    .iter()
    .map(|e| format!("data: {e}\n\n"))
    .collect::<String>();
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

struct Signed {
    dir: tempfile::TempDir,
    credentials: Arc<Credentials>,
}

impl Signed {
    fn new(stored: &Tokens) -> Signed {
        let dir = tempfile::tempdir().unwrap();
        let credentials = Arc::new(Credentials::with_keychain(dir.path(), None));
        credentials
            .set("chatgpt", "default", &stored.to_json())
            .unwrap();
        Signed { dir, credentials }
    }

    fn provider(&self, server: &MockServer) -> OpenAiResponses {
        let auth = ChatGptAuth::load(
            self.credentials.clone(),
            "default",
            OAuth::new(&server.uri()),
        )
        .unwrap()
        .expect("signed in");
        OpenAiResponses::chatgpt(
            format!("{}/backend-api/codex", server.uri()),
            Arc::new(auth),
        )
    }

    fn stored(&self) -> Tokens {
        Tokens::from_json(&self.credentials.get("chatgpt", "default").unwrap().unwrap()).unwrap()
    }
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "gpt-5.5".into(),
        system: "s".into(),
        messages: vec![Message::User {
            content: "hi".into(),
        }],
        options: RequestOptions {
            max_output_tokens: Some(1000),
            ..RequestOptions::default()
        },
        ..ChatRequest::default()
    }
}

async fn text_of(provider: &OpenAiResponses) -> Result<String, ProviderError> {
    let mut text = String::new();
    let mut stream = provider.stream(request());
    while let Some(event) = stream.next().await {
        if let ProviderEvent::TextDelta(delta) = event? {
            text.push_str(&delta);
        }
    }
    Ok(text)
}

#[tokio::test]
async fn requests_carry_the_token_and_the_account() {
    let server = MockServer::start().await;
    let token = access_token("a", 3600);
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", format!("Bearer {token}").as_str()))
        .and(header("chatgpt-account-id", "acct-123"))
        .and(header("originator", "codex_cli_rs"))
        .respond_with(text_reply())
        .expect(1)
        .mount(&server)
        .await;
    let signed = Signed::new(&tokens(&token, "rt-1"));
    let text = text_of(&signed.provider(&server)).await.unwrap();
    assert_eq!(text, "hi from chatgpt");
    let body: Value = server.received_requests().await.unwrap()[0]
        .body_json()
        .unwrap();
    assert_eq!(body["store"], false);
    // ChatGPT's backend takes no output limit.
    assert!(body.get("max_output_tokens").is_none(), "{body}");
}

// Spec: "Expired access token".
#[tokio::test]
async fn a_401_refreshes_the_token_and_retries_once() {
    let server = MockServer::start().await;
    let old = access_token("old", 3600);
    let new = access_token("new", 7200);
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", format!("Bearer {old}").as_str()))
        .respond_with(ResponseTemplate::new(401).set_body_string("token expired"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", format!("Bearer {new}").as_str()))
        .respond_with(text_reply())
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains(r#""refresh_token":"rt-1""#))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": new, "refresh_token": "rt-2"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let signed = Signed::new(&tokens(&old, "rt-1"));
    assert_eq!(
        text_of(&signed.provider(&server)).await.unwrap(),
        "hi from chatgpt"
    );
    // The new tokens are stored, for the next run.
    let stored = signed.stored();
    assert_eq!(stored.access_token, new);
    assert_eq!(stored.refresh_token, "rt-2");
    assert_eq!(stored.account_id.as_deref(), Some("acct-123"));
    drop(signed.dir);
}

#[tokio::test]
async fn a_token_about_to_expire_is_refreshed_first() {
    let server = MockServer::start().await;
    let old = access_token("old", 60);
    let new = access_token("new", 7200);
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", format!("Bearer {new}").as_str()))
        .respond_with(text_reply())
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": new})))
        .expect(1)
        .mount(&server)
        .await;
    let signed = Signed::new(&tokens(&old, "rt-1"));
    text_of(&signed.provider(&server)).await.unwrap();
    assert_eq!(signed.stored().refresh_token, "rt-1");
}

// Spec: "Two sessions refresh at once". Refresh tokens are used once: asking again with the
// same one would fail, so the tokens another process stored are used instead.
#[tokio::test]
async fn tokens_another_process_refreshed_are_used_without_asking_again() {
    let server = MockServer::start().await;
    let old = access_token("old", 3600);
    let theirs = access_token("theirs", 7200);
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", format!("Bearer {old}").as_str()))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", format!("Bearer {theirs}").as_str()))
        .respond_with(text_reply())
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({"error": {"code": "refresh_token_reused"}})),
        )
        .expect(0)
        .mount(&server)
        .await;
    let signed = Signed::new(&tokens(&old, "rt-1"));
    let provider = signed.provider(&server);
    // Another harness process refreshes after this one loaded the tokens.
    signed
        .credentials
        .set("chatgpt", "default", &tokens(&theirs, "rt-2").to_json())
        .unwrap();
    assert_eq!(text_of(&provider).await.unwrap(), "hi from chatgpt");
}

#[tokio::test]
async fn a_refused_refresh_says_to_sign_in_again() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({"error": {"code": "refresh_token_expired"}})),
        )
        .mount(&server)
        .await;
    let signed = Signed::new(&tokens(&access_token("old", 3600), "rt-1"));
    let error = text_of(&signed.provider(&server)).await.unwrap_err();
    assert!(
        error.to_string().contains("harness login chatgpt"),
        "{error}"
    );
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn a_second_401_is_an_error_not_a_loop() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .respond_with(ResponseTemplate::new(401).set_body_string("no"))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": access_token("new", 7200)})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let signed = Signed::new(&tokens(&access_token("old", 3600), "rt-1"));
    let error = text_of(&signed.provider(&server)).await.unwrap_err();
    assert!(
        matches!(error, ProviderError::Http { status: 401, .. }),
        "{error:?}"
    );
}

/// The credential store and nothing in the environment.
struct Stored(Arc<Credentials>);

impl Secrets for Stored {
    fn env(&self, _var: &str) -> Option<String> {
        None
    }

    fn credentials(&self) -> Option<Arc<Credentials>> {
        Some(self.0.clone())
    }
}

#[test]
fn chatgpt_models_need_a_signed_in_account() {
    let dir = tempfile::tempdir().unwrap();
    let credentials = Arc::new(Credentials::with_keychain(dir.path(), None));
    let none = BTreeMap::new();
    let error = resolve("chatgpt/gpt-5.5", &none, Stored(credentials.clone())).unwrap_err();
    assert_eq!(
        error,
        ResolveError::NotSignedIn {
            provider: "chatgpt".into(),
            profile: "default".into()
        }
    );
    assert!(
        error.to_string().contains("harness login chatgpt"),
        "{error}"
    );
    credentials
        .set("chatgpt", "default", &tokens("at", "rt").to_json())
        .unwrap();
    let r = resolve("chatgpt/gpt-5.5", &none, Stored(credentials.clone())).unwrap();
    assert_eq!(r.model, "gpt-5.5");
    assert_eq!(r.protocol, Protocol::OpenaiResponses);
    assert_eq!(r.base_url, "https://chatgpt.com/backend-api/codex");
    // Another profile is signed in separately.
    credentials.use_profile("chatgpt", "work").unwrap();
    let error = resolve("chatgpt/gpt-5.5", &none, Stored(credentials)).unwrap_err();
    assert!(error.to_string().contains("--profile work"), "{error}");
}
```

The CLI's tests need the feature declared (Step 6 lets it turn sign-in off), and `base64` to build tokens. In `crates/harness-cli/Cargo.toml`:

Replace (1 of 2):

```toml
name = "harness"
path = "src/main.rs"

[dependencies]
clap.workspace = true
harness-config.workspace = true
```

with:

```toml
name = "harness"
path = "src/main.rs"

[features]
default = ["chatgpt-login"]
# ChatGPT sign-in: `harness login chatgpt` and `chatgpt/*` models.
chatgpt-login = ["harness-providers/chatgpt-login"]

[dependencies]
clap.workspace = true
harness-config.workspace = true
```

Replace (2 of 2):

```toml

[dev-dependencies]
assert_cmd.workspace = true
harness-sandbox.workspace = true
nix.workspace = true
predicates.workspace = true
```

with:

```toml

[dev-dependencies]
assert_cmd.workspace = true
base64.workspace = true
harness-sandbox.workspace = true
nix.workspace = true
predicates.workspace = true
```

Create `crates/harness-cli/tests/login_e2e.rs`:

```rust
//! `harness login` against a mock OAuth server that also plays ChatGPT's backend. The tests use
//! the device flow, which needs no browser; tokens go to the file store.
#![cfg(feature = "chatgpt-login")]

use std::os::unix::fs::PermissionsExt;

use assert_cmd::Command;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use predicates::prelude::*;
use predicates::str::contains;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_harness");

fn jwt(claims: Value) -> String {
    let part = |value: &Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap());
    format!(
        "{}.{}.{}",
        part(&json!({"alg": "none"})),
        part(&claims),
        URL_SAFE_NO_PAD.encode(b"sig")
    )
}

/// The access token the mock server issues, valid until 2100.
fn access_token() -> String {
    jwt(json!({"exp": 4_102_444_800u64}))
}

/// A device-code sign-in that the user approves at once.
async fn mock_sign_in(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/usercode"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_auth_id": "device-auth-1",
            "user_code": "ABCD-1234",
            "interval": "0"
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "authorization_code": "code-1",
            "code_challenge": "challenge",
            "code_verifier": "verifier"
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id_token": jwt(json!({
                "email": "dev@example.com",
                "https://api.openai.com/auth": {"chatgpt_account_id": "acct-123"}
            })),
            "access_token": access_token(),
            "refresh_token": "rt-1"
        })))
        .mount(server)
        .await;
}

/// ChatGPT's backend: answers requests that carry the signed-in account.
async fn mock_backend(server: &MockServer) {
    let body = [
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": "hi from chatgpt"}),
        json!({"type": "response.completed", "response": {"status": "completed"}}),
    ]
    .iter()
    .map(|e| format!("data: {e}\n\n"))
    .collect::<String>();
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header(
            "authorization",
            format!("Bearer {}", access_token()).as_str(),
        ))
        .and(header("chatgpt-account-id", "acct-123"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(server)
        .await;
}

struct Env {
    home: TempDir,
    ws: TempDir,
    server_uri: String,
}

impl Env {
    fn new(server_uri: &str) -> Env {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir(ws.path().join(".git")).unwrap();
        Env {
            home,
            ws,
            server_uri: server_uri.to_string(),
        }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.current_dir(self.ws.path())
            .env("HARNESS_HOME", self.home.path())
            .env("HARNESS_CREDENTIAL_STORE", "file")
            .env("HARNESS_CHATGPT_ISSUER", &self.server_uri)
            .env(
                "HARNESS_CHATGPT_BASE_URL",
                format!("{}/backend-api/codex", self.server_uri),
            )
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("SSH_CONNECTION")
            .env_remove("SSH_TTY");
        cmd
    }

    fn credentials(&self) -> std::path::PathBuf {
        self.home.path().join("data/credentials.json")
    }
}

// Spec: "Sign-in over SSH" (with --device), then a turn on the account, and "Logout".
#[tokio::test(flavor = "multi_thread")]
async fn a_device_sign_in_lets_chatgpt_models_answer_until_logout() {
    let server = MockServer::start().await;
    mock_sign_in(&server).await;
    mock_backend(&server).await;
    let env = Env::new(&server.uri());
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["login", "chatgpt", "--device"])
            .assert()
            .success()
            .stderr(contains("not a contractual guarantee"))
            .stderr(contains("ABCD-1234"))
            .stderr(contains("/codex/device"))
            .stdout(contains(
                "Signed in to ChatGPT as dev@example.com (profile default)",
            ));
        let stored = std::fs::read_to_string(env.credentials()).unwrap();
        assert!(stored.contains("acct-123"), "{stored}");
        let mode = std::fs::metadata(env.credentials())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        env.cmd()
            .args(["--model", "chatgpt/gpt-5.5", "ask", "hi"])
            .assert()
            .success()
            .stdout(contains("hi from chatgpt"));
        env.cmd()
            .args(["logout", "chatgpt"])
            .assert()
            .success()
            .stdout(contains("Removed the stored credentials for chatgpt"));
        env.cmd()
            .args(["--model", "chatgpt/gpt-5.5", "ask", "hi"])
            .assert()
            .code(2)
            .stderr(contains("not signed in to chatgpt"))
            .stderr(contains("harness login chatgpt"));
    })
    .await
    .unwrap();
}

// Over SSH no browser can be opened here, so the device flow is used without --device.
#[tokio::test(flavor = "multi_thread")]
async fn over_ssh_the_device_flow_is_used() {
    let server = MockServer::start().await;
    mock_sign_in(&server).await;
    let env = Env::new(&server.uri());
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .env("SSH_CONNECTION", "10.0.0.2 50000 10.0.0.1 22")
            .args(["login", "chatgpt"])
            .assert()
            .success()
            .stderr(contains("ABCD-1234"));
    })
    .await
    .unwrap();
}

// Spec: "Switching ChatGPT accounts".
#[tokio::test(flavor = "multi_thread")]
async fn each_profile_signs_in_on_its_own() {
    let server = MockServer::start().await;
    mock_sign_in(&server).await;
    mock_backend(&server).await;
    let env = Env::new(&server.uri());
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["login", "chatgpt", "--device", "--profile", "work"])
            .assert()
            .success()
            .stdout(contains("(profile work)"));
        env.cmd()
            .args(["--model", "chatgpt/gpt-5.5", "ask", "hi"])
            .assert()
            .code(2)
            .stderr(contains("not signed in to chatgpt (profile default)"));
        env.cmd()
            .args(["auth", "use", "chatgpt", "work"])
            .assert()
            .success();
        env.cmd()
            .args(["--model", "chatgpt/gpt-5.5", "ask", "hi"])
            .assert()
            .success()
            .stdout(contains("hi from chatgpt"));
    })
    .await
    .unwrap();
}

// Spec: "Claude subscription credentials are never used": there is no Claude sign-in.
#[test]
fn only_chatgpt_can_be_signed_in_to() {
    let env = Env::new("http://127.0.0.1:9");
    for provider in ["anthropic", "claude"] {
        env.cmd()
            .args(["login", provider])
            .assert()
            .code(2)
            .stderr(contains("Claude Code"))
            .stderr(contains("harness auth add anthropic"));
    }
    env.cmd()
        .args(["login", "openai"])
        .assert()
        .code(2)
        .stderr(contains("harness auth add openai"));
    env.cmd()
        .args(["login", "nope"])
        .assert()
        .code(2)
        .stderr(contains("unknown provider `nope`"));
    assert!(!env.credentials().exists());
}

#[test]
fn help_lists_login_with_its_flags() {
    Command::new(BIN)
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("login"));
    Command::new(BIN)
        .args(["login", "--help"])
        .assert()
        .success()
        .stdout(contains("--device").and(contains("--profile")));
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p harness-core --test resilience; cargo test -p harness-providers --test chatgpt_provider; cargo test -p harness-cli --test login_e2e`
Expected: `resilience` FAILS to compile (``no method named `is_quota_exhausted` `` and ``no method named `resets_at` found for enum `ProviderError` ``). `chatgpt_provider` FAILS to compile: ``unresolved import `harness_providers::chatgpt::auth` ``, ``no associated function or constant named `chatgpt` found for struct `OpenAiResponses` ``, ``method `credentials` is not a member of trait `Secrets` ``, ``no variant named `NotSignedIn` found for enum `ResolveError` ``. `login_e2e` FAILS all 5 tests with `error: unrecognized subcommand 'login'` (clap suggests `logout`).

- [ ] **Step 3: Stop retrying exhausted quotas**

In `crates/harness-core/src/provider.rs`:

Replace:

```rust
}

impl ProviderError {
    /// Network errors, HTTP 429, and HTTP 5xx are worth retrying.
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::Network(_) => true,
            ProviderError::Http { status, .. } => *status == 429 || (500..600).contains(status),
            ProviderError::Protocol(_) | ProviderError::InStream(_) => false,
        }
    }

    /// Whether the provider rejected the request as longer than the model's context window.
    /// Providers say so in different words, so this looks for the usual phrases, in an error
    /// response (HTTP 400, 413 or 422) or an error the provider reported inside the stream. Never
```

with:

```rust
}

impl ProviderError {
    /// Network errors, HTTP 429, and HTTP 5xx are worth retrying; a 429 that reports an
    /// exhausted quota or plan limit is not, since waiting seconds does not end it.
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::Network(_) => true,
            ProviderError::Http { status: 429, .. } => !self.is_quota_exhausted(),
            ProviderError::Http { status, .. } => (500..600).contains(status),
            ProviderError::Protocol(_) | ProviderError::InStream(_) => false,
        }
    }

    /// Whether this is a 429 that reports an exhausted quota or plan limit: ChatGPT's
    /// `usage_limit_reached` and `usage_not_included`, or OpenAI's `insufficient_quota`.
    pub fn is_quota_exhausted(&self) -> bool {
        let ProviderError::Http {
            status: 429, body, ..
        } = self
        else {
            return false;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
            return false;
        };
        let error = &value["error"];
        [&error["type"], &error["code"]].iter().any(|v| {
            matches!(
                v.as_str(),
                Some("usage_limit_reached" | "usage_not_included" | "insufficient_quota")
            )
        })
    }

    /// When an exhausted limit resets, in seconds since the Unix epoch, if the provider said:
    /// `resets_at`, or `resets_in_seconds` from now.
    pub fn resets_at(&self) -> Option<u64> {
        let ProviderError::Http { body, .. } = self else {
            return None;
        };
        let value: serde_json::Value = serde_json::from_str(body).ok()?;
        let error = &value["error"];
        error["resets_at"].as_u64().or_else(|| {
            error["resets_in_seconds"]
                .as_u64()
                .map(|secs| crate::time::now_unix() + secs)
        })
    }

    /// Whether the provider rejected the request as longer than the model's context window.
    /// Providers say so in different words, so this looks for the usual phrases, in an error
    /// response (HTTP 400, 413 or 422) or an error the provider reported inside the stream. Never
```

In `crates/harness-core/src/agent.rs`:

Replace:

```rust
            wait.as_secs()
        );
    }
    match error {
        ProviderError::Http { status: 429, .. } => {
            format!(
```

with:

```rust
            wait.as_secs()
        );
    }
    if error.is_quota_exhausted() {
        let resets = error
            .resets_at()
            .map(|at| format!("; it resets at {}", crate::time::timestamp(at)))
            .unwrap_or_default();
        let body: String = match error {
            ProviderError::Http { body, .. } => body.chars().take(500).collect(),
            _ => String::new(),
        };
        return format!(
            "the provider's usage limit is reached{resets}. Switch models with --model (or /model in the terminal UI). HTTP 429: {body}"
        );
    }
    match error {
        ProviderError::Http { status: 429, .. } => {
            format!(
```

- [ ] **Step 4: The signed-in account and its refreshes**

Create `crates/harness-providers/src/chatgpt/auth.rs`:

```rust
//! The signed-in ChatGPT account a `chatgpt/*` model uses: its tokens, refreshed when the access
//! token expires within five minutes and after a 401, and stored again whenever they change.
//! Refresh tokens are single-use, so before asking for new tokens the stored ones are read
//! again: another harness process may already have refreshed them.

use std::{sync::Arc, time::Duration};

use harness_core::{provider::ProviderError, time::now_unix};
use tokio::sync::Mutex;

use super::oauth::{OAuth, OAuthError, Tokens};
use crate::credentials::{CredentialError, Credentials};

/// The provider name of the ChatGPT account.
pub const PROVIDER: &str = "chatgpt";
/// Where `chatgpt/*` requests go.
pub const BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
/// Refresh the access token when it expires within this long.
pub const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

pub struct ChatGptAuth {
    oauth: OAuth,
    credentials: Arc<Credentials>,
    profile: String,
    tokens: Mutex<Tokens>,
}

impl ChatGptAuth {
    /// The account signed in under `profile`, or `None` when there is none.
    pub fn load(
        credentials: Arc<Credentials>,
        profile: &str,
        oauth: OAuth,
    ) -> Result<Option<ChatGptAuth>, CredentialError> {
        let Some(tokens) = stored(&credentials, profile)? else {
            return Ok(None);
        };
        Ok(Some(ChatGptAuth {
            oauth,
            credentials,
            profile: profile.to_string(),
            tokens: Mutex::new(tokens),
        }))
    }

    /// The tokens for the next request, refreshed first when the access token is about to
    /// expire.
    pub async fn current(&self) -> Result<Tokens, ProviderError> {
        let mut tokens = self.tokens.lock().await;
        if expiring(&tokens) {
            let used = tokens.access_token.clone();
            self.renew(&mut tokens, &used).await?;
        }
        Ok(tokens.clone())
    }

    /// The tokens to retry with after the server refused `used` with a 401.
    pub async fn after_unauthorized(&self, used: &str) -> Result<Tokens, ProviderError> {
        let mut tokens = self.tokens.lock().await;
        // Another request of this process already renewed them.
        if tokens.access_token != used {
            return Ok(tokens.clone());
        }
        self.renew(&mut tokens, used).await?;
        Ok(tokens.clone())
    }

    /// Replaces `tokens`, whose access token `used` is no good: with the stored tokens when
    /// another process has renewed them, else with refreshed ones, which are then stored.
    async fn renew(&self, tokens: &mut Tokens, used: &str) -> Result<(), ProviderError> {
        if let Ok(Some(theirs)) = stored(&self.credentials, &self.profile)
            && theirs.access_token != used
        {
            *tokens = theirs;
            if !expiring(tokens) {
                return Ok(());
            }
        }
        let fresh = self.oauth.refresh(tokens).await.map_err(refresh_error)?;
        self.credentials
            .set(PROVIDER, &self.profile, &fresh.to_json())
            .map_err(|e| {
                ProviderError::Protocol(format!("cannot store the refreshed tokens: {e}"))
            })?;
        *tokens = fresh;
        Ok(())
    }
}

/// The tokens stored under `profile`, if they are there and readable.
fn stored(credentials: &Credentials, profile: &str) -> Result<Option<Tokens>, CredentialError> {
    Ok(credentials
        .get(PROVIDER, profile)?
        .as_deref()
        .and_then(Tokens::from_json))
}

/// Whether the access token expires within [`REFRESH_MARGIN`]. A token without an expiry is used
/// until the server refuses it.
fn expiring(tokens: &Tokens) -> bool {
    tokens
        .expires_at()
        .is_some_and(|at| at <= now_unix() + REFRESH_MARGIN.as_secs())
}

/// A failed refresh as the provider's error: an unreachable server can be retried; a refusal
/// means signing in again, and reads as the 401 it stands for.
fn refresh_error(error: OAuthError) -> ProviderError {
    match error {
        OAuthError::Network(message) => ProviderError::Network(message),
        other => ProviderError::Http {
            status: 401,
            body: other.to_string(),
            retry_after: None,
        },
    }
}
```

In `crates/harness-providers/src/chatgpt/mod.rs`:

Replace:

```rust
//! OpenAI's open-source Codex CLI (github.com/openai/codex, Apache-2.0, `codex-rs/login`), use
//! its public client, and identify to OpenAI as it (the Codex CLI's `originator` and scopes, not
//! a `harness`-specific identity). This is not an OpenAI endorsement of harness.

pub mod oauth;
```

with:

```rust
//! OpenAI's open-source Codex CLI (github.com/openai/codex, Apache-2.0, `codex-rs/login`), use
//! its public client, and identify to OpenAI as it (the Codex CLI's `originator` and scopes, not
//! a `harness`-specific identity). This is not an OpenAI endorsement of harness.

pub mod auth;
pub mod oauth;
```

In `crates/harness-providers/src/openai_responses.rs`, the provider takes an API key or an account:

Replace (1 of 2):

```rust
    }
}

/// A provider speaking the Responses protocol with an optional API key.
pub struct OpenAiResponses {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl OpenAiResponses {
```

with:

```rust
    }
}

/// How requests are authorized.
enum Auth {
    /// An API key, when the endpoint needs one.
    Key(Option<String>),
    /// A signed-in ChatGPT account.
    #[cfg(feature = "chatgpt-login")]
    ChatGpt(std::sync::Arc<crate::chatgpt::auth::ChatGptAuth>),
}

/// A provider speaking the Responses protocol, with an API key or a ChatGPT account.
pub struct OpenAiResponses {
    client: reqwest::Client,
    base_url: String,
    auth: Auth,
}

impl OpenAiResponses {
```

Replace (2 of 2):

```rust
        OpenAiResponses {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
        }
    }
}

impl Provider for OpenAiResponses {
    fn stream(&self, request: ChatRequest) -> ProviderStream {
        let mut http = self
            .client
            .post(format!("{}/responses", self.base_url))
            .json(&request_body(&request));
        if let Some(key) = &self.api_key {
            http = http.bearer_auth(key);
        }
        sse::events(sse::send(http), ResponsesStreamParser::default())
    }
}
```

with:

```rust
        OpenAiResponses {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            auth: Auth::Key(api_key),
        }
    }

    /// ChatGPT's backend, as the signed-in account `auth`.
    #[cfg(feature = "chatgpt-login")]
    pub fn chatgpt(
        base_url: impl Into<String>,
        auth: std::sync::Arc<crate::chatgpt::auth::ChatGptAuth>,
    ) -> Self {
        OpenAiResponses {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            auth: Auth::ChatGpt(auth),
        }
    }
}

impl Provider for OpenAiResponses {
    fn stream(&self, request: ChatRequest) -> ProviderStream {
        let url = format!("{}/responses", self.base_url);
        match &self.auth {
            Auth::Key(key) => {
                let mut http = self.client.post(url).json(&request_body(&request));
                if let Some(key) = key {
                    http = http.bearer_auth(key);
                }
                sse::events(sse::send(http), ResponsesStreamParser::default())
            }
            #[cfg(feature = "chatgpt-login")]
            Auth::ChatGpt(auth) => {
                let mut body = request_body(&request);
                // ChatGPT's backend takes no output limit (Codex never sends one).
                if let Some(object) = body.as_object_mut() {
                    object.remove("max_output_tokens");
                }
                let (client, auth) = (self.client.clone(), auth.clone());
                // A 401 renews the tokens once, and the request is sent once more.
                let response = async move {
                    let tokens = auth.current().await?;
                    let first = sse::send(chatgpt_request(&client, &url, &body, &tokens)).await?;
                    if first.status() != reqwest::StatusCode::UNAUTHORIZED {
                        return Ok(first);
                    }
                    let tokens = auth.after_unauthorized(&tokens.access_token).await?;
                    sse::send(chatgpt_request(&client, &url, &body, &tokens)).await
                };
                sse::events(response, ResponsesStreamParser::default())
            }
        }
    }
}

/// A request to ChatGPT's backend: the access token, and the account it belongs to.
#[cfg(feature = "chatgpt-login")]
fn chatgpt_request(
    client: &reqwest::Client,
    url: &str,
    body: &Value,
    tokens: &crate::chatgpt::oauth::Tokens,
) -> reqwest::RequestBuilder {
    let mut request = client
        .post(url)
        .bearer_auth(&tokens.access_token)
        .header("originator", crate::chatgpt::oauth::ORIGINATOR)
        .json(body);
    if let Some(account) = &tokens.account_id {
        request = request.header("ChatGPT-Account-ID", account);
    }
    request
}
```

- [ ] **Step 5: Resolve `chatgpt/*` models**

In `crates/harness-providers/src/registry.rs`:

Replace (1 of 7):

```rust
use std::{collections::BTreeMap, sync::Arc};

use harness_config::config::{Protocol, ProviderConfig};
use harness_core::provider::Provider;

```

with:

```rust
use std::{collections::BTreeMap, sync::Arc};

use crate::credentials::Credentials;

use harness_config::config::{Protocol, ProviderConfig};
use harness_core::provider::Provider;

```

Replace (2 of 7):

```rust
}

/// Providers usable without configuration.
pub const BUILTIN_PROVIDERS: [Builtin; 6] = [
    builtin(
        "ollama",
        Protocol::OpenaiChat,
```

with:

```rust
}

/// Providers usable without configuration.
pub const BUILTIN_PROVIDERS: [Builtin; 7] = [
    builtin(
        "ollama",
        Protocol::OpenaiChat,
```

Replace (3 of 7):

```rust
        "https://api.anthropic.com/v1",
        Some("ANTHROPIC_API_KEY"),
    ),
];

const LOCAL_PROVIDERS: [&str; 3] = ["ollama", "lmstudio", "llamacpp"];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
```

with:

```rust
        "https://api.anthropic.com/v1",
        Some("ANTHROPIC_API_KEY"),
    ),
    // Signed in with `harness login chatgpt`, not a key.
    builtin(
        "chatgpt",
        Protocol::OpenaiResponses,
        "https://chatgpt.com/backend-api/codex",
        None,
    ),
];

/// The built-in provider a ChatGPT account answers for.
pub const CHATGPT: &str = "chatgpt";

const LOCAL_PROVIDERS: [&str; 3] = ["ollama", "lmstudio", "llamacpp"];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
```

Replace (4 of 7):

```rust
        "the key for `{provider}` is a Claude subscription token, which only Claude Code may use; harness needs an Anthropic API key (from console.anthropic.com)"
    )]
    SubscriptionToken { provider: String },
}

/// Where API keys come from: environment variables, and keys stored with `harness auth add`.
```

with:

```rust
        "the key for `{provider}` is a Claude subscription token, which only Claude Code may use; harness needs an Anthropic API key (from console.anthropic.com)"
    )]
    SubscriptionToken { provider: String },
    #[error("not signed in to {provider} (profile {profile}); run `{}`", login_command(.provider, .profile))]
    NotSignedIn { provider: String, profile: String },
    #[error("this build of harness was made without ChatGPT sign-in (the `chatgpt-login` feature)")]
    SignInUnavailable,
}

/// The command that signs in to `provider` under `profile`.
fn login_command(provider: &str, profile: &str) -> String {
    if profile == crate::credentials::DEFAULT_PROFILE {
        format!("harness login {provider}")
    } else {
        format!("harness login {provider} --profile {profile}")
    }
}

/// Where API keys come from: environment variables, and keys stored with `harness auth add`.
```

Replace (5 of 7):

```rust
    fn stored(&self, _provider: &str) -> Option<String> {
        None
    }
}

impl<F: Fn(&str) -> Option<String>> Secrets for F {
```

with:

```rust
    fn stored(&self, _provider: &str) -> Option<String> {
        None
    }

    /// The credential store, for providers that sign in (`chatgpt`).
    fn credentials(&self) -> Option<Arc<Credentials>> {
        None
    }
}

impl<F: Fn(&str) -> Option<String>> Secrets for F {
```

Replace (6 of 7):

```rust
        .split_once('/')
        .filter(|(p, m)| !p.is_empty() && !m.is_empty())
        .ok_or_else(|| ResolveError::BadId(model_id.to_string()))?;
    let (protocol, base_url, key_env) = if let Some(cfg) = providers.get(name) {
        (cfg.protocol, cfg.base_url.clone(), cfg.api_key_env.clone())
    } else if let Some(builtin) = BUILTIN_PROVIDERS.iter().find(|b| b.name == name) {
```

with:

```rust
        .split_once('/')
        .filter(|(p, m)| !p.is_empty() && !m.is_empty())
        .ok_or_else(|| ResolveError::BadId(model_id.to_string()))?;
    if name == CHATGPT && !providers.contains_key(name) {
        return chatgpt(model_id, model, &secrets);
    }
    let (protocol, base_url, key_env) = if let Some(cfg) = providers.get(name) {
        (cfg.protocol, cfg.base_url.clone(), cfg.api_key_env.clone())
    } else if let Some(builtin) = BUILTIN_PROVIDERS.iter().find(|b| b.name == name) {
```

Replace (7 of 7):

```rust
    })
}

/// The three local servers, except any the user has redefined in config.
pub fn local_endpoints(providers: &BTreeMap<String, ProviderConfig>) -> Vec<Endpoint> {
    BUILTIN_PROVIDERS
```

with:

```rust
    })
}

/// A `chatgpt/*` model, for the account signed in under the provider's active profile.
/// `HARNESS_CHATGPT_ISSUER` and `HARNESS_CHATGPT_BASE_URL` point sign-in and requests elsewhere,
/// for tests.
#[cfg(feature = "chatgpt-login")]
fn chatgpt(model_id: &str, model: &str, secrets: &impl Secrets) -> Result<Resolved, ResolveError> {
    use crate::chatgpt::{
        auth::{BASE_URL, ChatGptAuth},
        oauth::{ISSUER, OAuth},
    };
    let credentials = secrets.credentials();
    let profile = credentials
        .as_ref()
        .and_then(|c| c.active_profile(CHATGPT).ok())
        .unwrap_or_else(|| crate::credentials::DEFAULT_PROFILE.to_string());
    let not_signed_in = || ResolveError::NotSignedIn {
        provider: CHATGPT.to_string(),
        profile: profile.clone(),
    };
    let credentials = credentials.ok_or_else(not_signed_in)?;
    let issuer = secrets
        .env("HARNESS_CHATGPT_ISSUER")
        .unwrap_or_else(|| ISSUER.to_string());
    let auth = ChatGptAuth::load(credentials, &profile, OAuth::new(&issuer))
        .ok()
        .flatten()
        .ok_or_else(not_signed_in)?;
    let base_url = secrets
        .env("HARNESS_CHATGPT_BASE_URL")
        .unwrap_or_else(|| BASE_URL.to_string());
    Ok(Resolved {
        provider: Arc::new(OpenAiResponses::chatgpt(base_url.clone(), Arc::new(auth))),
        model: model.to_string(),
        id: model_id.to_string(),
        protocol: Protocol::OpenaiResponses,
        base_url,
        api_key: None,
    })
}

#[cfg(not(feature = "chatgpt-login"))]
fn chatgpt(
    _model_id: &str,
    _model: &str,
    _secrets: &impl Secrets,
) -> Result<Resolved, ResolveError> {
    Err(ResolveError::SignInUnavailable)
}

/// The three local servers, except any the user has redefined in config.
pub fn local_endpoints(providers: &BTreeMap<String, ProviderConfig>) -> Vec<Endpoint> {
    BUILTIN_PROVIDERS
```

- [ ] **Step 6: Add `harness login`**

In `Cargo.toml`, the CLI decides whether sign-in is built:

Replace:

```toml
harness-config = { path = "crates/harness-config" }
harness-context = { path = "crates/harness-context" }
harness-core = { path = "crates/harness-core" }
harness-providers = { path = "crates/harness-providers" }
harness-sandbox = { path = "crates/harness-sandbox" }
harness-shell = { path = "crates/harness-shell" }
harness-tools = { path = "crates/harness-tools" }
```

with:

```toml
harness-config = { path = "crates/harness-config" }
harness-context = { path = "crates/harness-context" }
harness-core = { path = "crates/harness-core" }
harness-providers = { path = "crates/harness-providers", default-features = false }
harness-sandbox = { path = "crates/harness-sandbox" }
harness-shell = { path = "crates/harness-shell" }
harness-tools = { path = "crates/harness-tools" }
```

Create `crates/harness-cli/src/login.rs`:

```rust
//! `harness login <provider>`: ChatGPT sign-in, in the browser or with a device code. Claude
//! subscriptions cannot be signed in to: Anthropic allows them only in Claude Code.

use harness_providers::registry::BUILTIN_PROVIDERS;

use crate::{setup::Setup, term::terminal_safe};

/// What every ChatGPT sign-in says first.
#[cfg(feature = "chatgpt-login")]
pub const NOTICE: &str = "Signing in with ChatGPT lets harness use the models your ChatGPT plan includes. OpenAI allows this in third-party tools today, but that is its current practice, not a contractual guarantee: it can change at any time.";

/// `harness login <provider> [--profile <name>] [--device]`.
pub async fn run(provider: &str, profile: &str, device: bool) -> u8 {
    let setup = match crate::setup::load() {
        Ok(setup) => setup,
        Err(message) => {
            eprintln!("error: {}", terminal_safe(&message));
            return 2;
        }
    };
    match provider {
        "chatgpt" => sign_in(&setup, profile, device).await,
        "anthropic" | "claude" => {
            eprintln!(
                "error: harness cannot sign in to Claude: Anthropic allows Claude Free, Pro and Max plans only in Claude Code. Use an Anthropic API key instead: `harness auth add anthropic`."
            );
            2
        }
        other if takes_a_key(&setup, other) => {
            eprintln!("error: {other} takes an API key, not a sign-in: `harness auth add {other}`");
            2
        }
        other
            if setup.config.providers.contains_key(other)
                || BUILTIN_PROVIDERS.iter().any(|b| b.name == other) =>
        {
            eprintln!("error: {other} needs no sign-in");
            2
        }
        other => {
            eprintln!(
                "error: unknown provider `{}`; `harness login` signs in to chatgpt",
                terminal_safe(other)
            );
            2
        }
    }
}

#[cfg(not(feature = "chatgpt-login"))]
async fn sign_in(_setup: &Setup, _profile: &str, _device: bool) -> u8 {
    eprintln!(
        "error: this build of harness was made without ChatGPT sign-in (the `chatgpt-login` feature)"
    );
    2
}

#[cfg(feature = "chatgpt-login")]
async fn sign_in(setup: &Setup, profile: &str, device: bool) -> u8 {
    use harness_providers::{
        chatgpt::oauth::{ISSUER, OAuth},
        credentials,
        registry::CHATGPT,
    };
    if let Err(e) = credentials::check_name("profile", profile) {
        eprintln!("error: {}", terminal_safe(&e.to_string()));
        return 2;
    }
    eprintln!("{NOTICE}");
    // A test hook: a mock authorization server.
    let issuer = crate::setup::env("HARNESS_CHATGPT_ISSUER").unwrap_or_else(|| ISSUER.to_string());
    let oauth = OAuth::new(&issuer);
    let device = device || wants_device_flow(crate::setup::env);
    let tokens = tokio::select! {
        tokens = flows::sign_in(&oauth, device) => tokens,
        _ = tokio::signal::ctrl_c() => {
            eprintln!("sign-in cancelled");
            return 130;
        }
    };
    let tokens = match tokens {
        Ok(tokens) => tokens,
        Err(e) => {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
            return 1;
        }
    };
    match setup.credentials.set(CHATGPT, profile, &tokens.to_json()) {
        Ok(place) => {
            for warning in setup.credentials.take_warnings() {
                eprintln!("warning: {}", terminal_safe(&warning));
            }
            let who = tokens
                .email
                .as_deref()
                .map(|email| format!(" as {}", terminal_safe(email)))
                .unwrap_or_default();
            println!(
                "Signed in to ChatGPT{who} (profile {profile}); the tokens are in {}.",
                terminal_safe(&place)
            );
            println!("Use a model your plan includes with --model chatgpt/<model>.");
            0
        }
        Err(e) => {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
            1
        }
    }
}

/// Whether `provider` authenticates with an API key.
fn takes_a_key(setup: &Setup, provider: &str) -> bool {
    match setup.config.providers.get(provider) {
        Some(cfg) => cfg.api_key_env.is_some(),
        None => BUILTIN_PROVIDERS
            .iter()
            .any(|b| b.name == provider && b.key_env.is_some()),
    }
}

/// Whether no browser can be opened here: over SSH, or on Linux without a display.
#[cfg(feature = "chatgpt-login")]
pub fn wants_device_flow(env: impl Fn(&str) -> Option<String>) -> bool {
    let set = |var: &str| env(var).is_some_and(|value| !value.is_empty());
    if set("SSH_CONNECTION") || set("SSH_TTY") {
        return true;
    }
    cfg!(target_os = "linux") && !set("DISPLAY") && !set("WAYLAND_DISPLAY")
}

#[cfg(feature = "chatgpt-login")]
mod flows {
    use std::time::Duration;

    use harness_providers::chatgpt::oauth::{
        CALLBACK_PORTS, CallbackServer, DEVICE_CODE_WAIT, OAuth, OAuthError, Pkce, Tokens,
        random_state,
    };

    use crate::term::terminal_safe;

    /// How long the browser flow waits for the user.
    const BROWSER_WAIT: Duration = Duration::from_secs(10 * 60);

    /// Signs in in the browser, or with a device code when `device` is set or no browser opens.
    pub async fn sign_in(oauth: &OAuth, device: bool) -> Result<Tokens, OAuthError> {
        if !device {
            match browser(oauth).await {
                Ok(tokens) => return Ok(tokens),
                Err(Browser::CannotOpen(why)) => eprintln!(
                    "cannot open a browser ({}); signing in with a device code instead",
                    terminal_safe(&why)
                ),
                Err(Browser::Failed(e)) => return Err(e),
            }
        }
        let code = oauth.request_device_code().await?;
        eprintln!(
            "To sign in, open {} in a browser and enter the code {} (it expires in 15 minutes).\nOnly enter it if you started this sign-in yourself.",
            terminal_safe(&code.verification_url),
            terminal_safe(&code.user_code)
        );
        oauth.poll_device_code(&code, DEVICE_CODE_WAIT).await
    }

    enum Browser {
        CannotOpen(String),
        Failed(OAuthError),
    }

    impl From<OAuthError> for Browser {
        fn from(error: OAuthError) -> Browser {
            Browser::Failed(error)
        }
    }

    impl From<std::io::Error> for Browser {
        fn from(error: std::io::Error) -> Browser {
            Browser::Failed(error.into())
        }
    }

    async fn browser(oauth: &OAuth) -> Result<Tokens, Browser> {
        let callback = CallbackServer::bind(&CALLBACK_PORTS).await?;
        let pkce = Pkce::generate()?;
        let state = random_state()?;
        let url = oauth.authorize_url(&callback.redirect_uri(), &pkce, &state);
        open(&url)
            .await
            .map_err(|e| Browser::CannotOpen(e.to_string()))?;
        eprintln!(
            "Sign in in the browser window that opened. If none did, open:\n  {}",
            terminal_safe(&url)
        );
        let code = tokio::time::timeout(BROWSER_WAIT, callback.wait_for_code(&state))
            .await
            .map_err(|_| OAuthError::TimedOut)??;
        Ok(oauth
            .exchange_code(&code, &callback.redirect_uri(), &pkce.verifier)
            .await?)
    }

    /// Opens `url` in the default browser.
    async fn open(url: &str) -> std::io::Result<()> {
        let program = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let status = tokio::process::Command::new(program)
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!("{program} failed: {status}")))
        }
    }
}

#[cfg(all(test, feature = "chatgpt-login"))]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |var| {
            pairs
                .iter()
                .find(|(k, _)| *k == var)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn ssh_sessions_use_the_device_flow() {
        assert!(wants_device_flow(env(&[("SSH_CONNECTION", "a b c d")])));
        assert!(wants_device_flow(env(&[("SSH_TTY", "/dev/pts/1")])));
    }

    #[test]
    fn a_display_decides_on_linux() {
        let with_display = wants_device_flow(env(&[("DISPLAY", ":0")]));
        assert!(!with_display);
        assert!(!wants_device_flow(env(&[("WAYLAND_DISPLAY", "wayland-0")])));
        // macOS always has a browser; Linux without a display has none.
        assert_eq!(wants_device_flow(env(&[])), cfg!(target_os = "linux"));
    }
}
```

In `crates/harness-cli/src/setup.rs`:

Replace (1 of 5):

```rust
use std::path::PathBuf;

use harness_config::{
    config::{self, Config},
```

with:

```rust
use std::{path::PathBuf, sync::Arc};

use harness_config::{
    config::{self, Config},
```

Replace (2 of 5):

```rust
    /// The workspaces the user trusts.
    pub trust: TrustStore,
    /// Stored API keys and sign-in tokens.
    pub credentials: Credentials,
}

impl Setup {
```

with:

```rust
    /// The workspaces the user trusts.
    pub trust: TrustStore,
    /// Stored API keys and sign-in tokens.
    pub credentials: Arc<Credentials>,
}

impl Setup {
```

Replace (3 of 5):

```rust
/// API keys from the environment, then from the credential store (`harness auth add`).
#[derive(Clone, Copy)]
pub struct Keys<'a> {
    credentials: &'a Credentials,
}

impl Secrets for Keys<'_> {
```

with:

```rust
/// API keys from the environment, then from the credential store (`harness auth add`).
#[derive(Clone, Copy)]
pub struct Keys<'a> {
    credentials: &'a Arc<Credentials>,
}

impl Secrets for Keys<'_> {
```

Replace (4 of 5):

```rust
            }
        }
    }
}

/// Loads paths and configuration. Errors are user-facing messages (exit code 2).
```

with:

```rust
            }
        }
    }

    fn credentials(&self) -> Option<Arc<Credentials>> {
        Some(self.credentials.clone())
    }
}

/// Loads paths and configuration. Errors are user-facing messages (exit code 2).
```

Replace (5 of 5):

```rust
    for warning in &config.warnings {
        eprintln!("warning: {}", crate::term::terminal_safe(warning));
    }
    let credentials = Credentials::open(&paths.data_dir, env);
    Ok(Setup {
        paths,
        config,
```

with:

```rust
    for warning in &config.warnings {
        eprintln!("warning: {}", crate::term::terminal_safe(warning));
    }
    let credentials = Arc::new(Credentials::open(&paths.data_dir, env));
    Ok(Setup {
        paths,
        config,
```

In `crates/harness-cli/src/main.rs`:

Replace (1 of 4):

```rust
mod auth;
mod context;
mod doctor;
mod models;
mod prompt;
mod sandbox;
```

with:

```rust
mod auth;
mod context;
mod doctor;
mod login;
mod models;
mod prompt;
mod sandbox;
```

Replace (2 of 4):

```rust
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Remove a provider's stored credentials
    Logout {
        /// The provider, e.g. openai
```

with:

```rust
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Sign in to ChatGPT, in the browser or with a device code
    Login {
        /// The provider to sign in to: chatgpt
        provider: String,
        /// The account profile to sign in under
        #[arg(long, default_value = "default")]
        profile: String,
        /// Sign in with a device code instead of the browser (for SSH sessions)
        #[arg(long)]
        device: bool,
    },
    /// Remove a provider's stored credentials
    Logout {
        /// The provider, e.g. openai
```

Replace (3 of 4):

```rust
        Command::Auth {
            command: AuthCommand::Use { .. },
        } => "harness auth use",
        Command::Logout { .. } => "harness logout",
        Command::Trust { .. } => "harness trust",
        Command::Sandbox { .. } => "harness sandbox doctor",
```

with:

```rust
        Command::Auth {
            command: AuthCommand::Use { .. },
        } => "harness auth use",
        Command::Login { .. } => "harness login",
        Command::Logout { .. } => "harness logout",
        Command::Trust { .. } => "harness trust",
        Command::Sandbox { .. } => "harness sandbox doctor",
```

Replace (4 of 4):

```rust
            Some(Command::Auth {
                command: AuthCommand::Use { provider, profile },
            }) => auth::use_profile(&provider, &profile),
            Some(Command::Logout { provider, profile }) => {
                auth::logout(&provider, profile.as_deref())
            }
```

with:

```rust
            Some(Command::Auth {
                command: AuthCommand::Use { provider, profile },
            }) => auth::use_profile(&provider, &profile),
            Some(Command::Login {
                provider,
                profile,
                device,
            }) => login::run(&provider, &profile, device).await,
            Some(Command::Logout { provider, profile }) => {
                auth::logout(&provider, profile.as_deref())
            }
```

- [ ] **Step 7: Run the tests**

Run: `cargo test -p harness-core -p harness-providers -p harness-cli`
Expected: PASS: 12 in `resilience` (among them `a_usage_limit_ends_the_turn_with_its_reset_time`), 7 in `chatgpt_provider` (among them `tokens_another_process_refreshed_are_used_without_asking_again`), 5 in `login_e2e`, and `login`'s 2 unit tests; 491 tests in the three crates.

- [ ] **Step 8: Lint, with and without the feature**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo clippy -p harness-providers --no-default-features --all-targets -- -D warnings && cargo clippy -p harness-cli --no-default-features --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 9: Check a build without sign-in**

Run: `HARNESS_HOME=$(mktemp -d) HARNESS_CREDENTIAL_STORE=file cargo run -q -p harness-cli --no-default-features -- login chatgpt; echo "exit $?"`
Expected: `error: this build of harness was made without ChatGPT sign-in (the `chatgpt-login` feature)` and `exit 2`.

- [ ] **Step 10: Commit**

```bash
git add Cargo.toml Cargo.lock crates/harness-core crates/harness-providers crates/harness-cli
git commit -F - <<'EOF'
feat(cli): sign in to ChatGPT and use chatgpt/* models

harness login chatgpt shows that ChatGPT use in third-party tools is
OpenAI's current practice, then signs in in the browser, or with a
device code with --device, over SSH, or when no browser opens, and
stores the tokens under the profile. chatgpt/* models go to ChatGPT's
backend with the account header; the access token is refreshed when it
expires within five minutes and after a 401, once, using tokens
another process stored when there are some. harness login refuses
Claude. A 429 that reports an exhausted quota or plan limit is not
retried, and the error names its reset time. The chatgpt-login feature
(on by default) can leave all of it out of a build.

<trailer lines from the controller>
EOF
```

---

### Task 7: Model profiles

**Files:**
- Create: `crates/harness-providers/src/profiles.rs`, `crates/harness-providers/tests/profiles.rs`, `crates/harness-cli/tests/profiles_e2e.rs`
- Modify: `Cargo.lock` (cargo updates it), `crates/harness-config/Cargo.toml`, `crates/harness-config/src/config.rs`, `crates/harness-config/tests/config.rs`, `crates/harness-core/src/tokens.rs`, `crates/harness-providers/Cargo.toml`, `crates/harness-providers/src/lib.rs`, `crates/harness-providers/src/registry.rs`, `crates/harness-providers/src/openai_chat.rs`, `crates/harness-providers/tests/openai_chat_parser.rs`, `crates/harness-cli/src/ask.rs`, `crates/harness-cli/src/context.rs`, `crates/harness-cli/tests/compaction_e2e.rs`, `crates/harness-cli/tests/context_e2e.rs`, `crates/harness-cli/tests/sandbox_e2e.rs`

**Interfaces:**
- Consumes: `RequestOptions`, `AgentConfig::request` (Task 2), `Resolved::base_url` (Task 2).
- Produces:
  - `harness_config::config::ProfileSettings { context_window, min_context, max_output_tokens: Option<u64>, temperature: Option<f64>, reasoning_effort: Option<String>, text_tool_calls, local: Option<bool> }` with `overlaid(&self, other)`; `ConfigFile::profiles` and `Config::profiles`, both `BTreeMap<String, ProfileSettings>`. `ConfigFile` and `Config` derive `PartialEq` without `Eq` now (a temperature is a float). A project's profiles are widening settings, listed as `profiles."<glob>": <key> = <value>, …`.
  - `harness_providers::profiles::{DEFAULT_MIN_CONTEXT = 32_768, FALLBACK_CONTEXT_WINDOW = 8_192, ModelProfile, builtin_profiles, resolve, is_local}`: `resolve(model_id: &str, local: bool, user: &BTreeMap<String, ProfileSettings>) -> ModelProfile`, `ModelProfile::request_options() -> RequestOptions`, `is_local(model_id, base_url) -> bool`. `ModelProfile { context_window: Option<u64>, min_context: u64, max_output_tokens, temperature, reasoning_effort, text_tool_calls: bool, local: bool }`.
  - `registry::LOCAL_PROVIDERS` is public.
  - The chat adapter sends the profile's `max_tokens`, `temperature` and `reasoning_effort`.
  - In the CLI: `context::system_prompt(setup, base, context_window: u64)`; `ask` sets `AgentConfig::{context_window, request}` from the profile.

This replaces P3's one assumed window (P3's known limitation "One context window for every model"). The e2e suites that measure against P3's 32,768 tokens, and one that counts warnings, get a profile for their mock provider.

- [ ] **Step 1: Write the failing tests**

Append to `crates/harness-config/tests/config.rs`, after a blank line:

```rust
#[test]
fn model_profiles_are_read_from_the_global_config() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global.toml");
    std::fs::write(
        &global,
        "[profiles.\"ollama/qwen3-coder*\"]\ncontext_window = 65536\ntemperature = 0.2\ntext_tool_calls = false\n\n[profiles.\"openai/*\"]\nreasoning_effort = \"high\"\nmax_output_tokens = 8000\n",
    )
    .unwrap();
    let cfg = config::load(&global, dir.path(), &TrustStore::default()).unwrap();
    let qwen = &cfg.profiles["ollama/qwen3-coder*"];
    assert_eq!(qwen.context_window, Some(65_536));
    assert_eq!(qwen.temperature, Some(0.2));
    assert_eq!(qwen.text_tool_calls, Some(false));
    assert_eq!(qwen.local, None);
    let openai = &cfg.profiles["openai/*"];
    assert_eq!(openai.reasoning_effort.as_deref(), Some("high"));
    assert_eq!(openai.max_output_tokens, Some(8000));
}

// A project's profiles choose output limits, reasoning effort and context budgets, which cost
// paid requests, and whether text runs as tool calls: they need trust.
#[test]
fn a_projects_model_profiles_need_trust() {
    let global = "[profiles.\"ollama/*\"]\ncontext_window = 32768\ntemperature = 0.5\n";
    let project = "[profiles.\"ollama/*\"]\ntemperature = 0.1\n[profiles.\"openai/*\"]\nreasoning_effort = \"high\"\n";
    let (cfg, widening) = load_project(Some(global), project, true);
    assert_eq!(cfg.profiles.len(), 1);
    assert_eq!(cfg.profiles["ollama/*"].temperature, Some(0.5));
    let widening = widening.expect("profiles widen");
    assert_eq!(
        widening.items,
        [
            "profiles.\"ollama/*\": temperature = 0.1",
            "profiles.\"openai/*\": reasoning_effort = \"high\""
        ]
    );
    assert_eq!(cfg.warnings.len(), 1, "{:?}", cfg.warnings);
    assert!(cfg.warnings[0].contains("profiles"), "{:?}", cfg.warnings);

    // Trusted, a project's fields go over the global profile's.
    let dir = tempfile::tempdir().unwrap();
    let global_file = dir.path().join("global.toml");
    std::fs::write(&global_file, global).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(ws.join(".harness")).unwrap();
    std::fs::write(ws.join(".harness/config.toml"), project).unwrap();
    let mut trust = TrustStore::load(&dir.path().join("data")).unwrap();
    trust
        .trust(&ws, &widening_of(&global_file, &ws).fingerprint)
        .unwrap();
    let cfg = config::load(&global_file, &ws, &trust).unwrap();
    assert_eq!(cfg.profiles["ollama/*"].temperature, Some(0.1));
    assert_eq!(cfg.profiles["ollama/*"].context_window, Some(32_768));
    assert_eq!(
        cfg.profiles["openai/*"].reasoning_effort.as_deref(),
        Some("high")
    );
}

#[test]
fn invalid_profiles_are_errors_naming_the_file() {
    for (text, problem) in [
        (
            "[profiles.\"ollama/[qwen\"]\ntemperature = 0.2\n",
            "not a valid glob",
        ),
        (
            "[profiles.\"ollama/*\"]\ncontext_window = 0\n",
            "context_window",
        ),
        (
            "[profiles.\"ollama/*\"]\ntemperature = 3.0\n",
            "temperature",
        ),
        (
            "[profiles.\"ollama/*\"]\ncontxt_window = 1\n",
            "contxt_window",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        std::fs::write(&global, text).unwrap();
        let error = config::load(&global, dir.path(), &TrustStore::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("global.toml"), "{error}");
        assert!(error.contains(problem), "{error}");
    }
}
```

Create `crates/harness-providers/tests/profiles.rs`:

```rust
//! Model profiles: the user's settings, then the built-in profiles, then protocol defaults, each
//! setting on its own, the most specific key winning within a layer.

use std::collections::BTreeMap;

use harness_config::config::ProfileSettings;
use harness_core::message::RequestOptions;
use harness_providers::profiles::{
    DEFAULT_MIN_CONTEXT, ModelProfile, builtin_profiles, is_local, resolve,
};

fn user(entries: &[(&str, ProfileSettings)]) -> BTreeMap<String, ProfileSettings> {
    entries
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn temperature(t: f64) -> ProfileSettings {
    ProfileSettings {
        temperature: Some(t),
        ..ProfileSettings::default()
    }
}

fn window(tokens: u64) -> ProfileSettings {
    ProfileSettings {
        context_window: Some(tokens),
        ..ProfileSettings::default()
    }
}

// Spec: "User profile overrides built-in".
#[test]
fn the_users_profile_overrides_the_builtin_one() {
    let builtin = resolve("ollama/qwen3-coder:30b", true, &BTreeMap::new());
    assert_eq!(builtin.temperature, Some(0.7));
    let mine = user(&[("ollama/qwen3-coder*", temperature(0.2))]);
    let profile = resolve("ollama/qwen3-coder:30b", true, &mine);
    assert_eq!(profile.temperature, Some(0.2));
    // Settings the user left alone still come from the built-in profile.
    assert_eq!(profile.context_window, builtin.context_window);
}

// Spec: "The most specific key wins".
#[test]
fn the_most_specific_key_wins() {
    let mine = user(&[
        ("ollama/*", window(65_536)),
        ("ollama/qwen3-coder*", window(16_384)),
    ]);
    assert_eq!(
        resolve("ollama/qwen3-coder:30b", true, &mine).context_window,
        Some(16_384)
    );
    assert_eq!(
        resolve("ollama/llama3.1", true, &mine).context_window,
        Some(65_536)
    );
    // The same rule holds among the built-in profiles: qwen3-coder over qwen3.
    let none = BTreeMap::new();
    assert_eq!(
        resolve("ollama/qwen3:14b", true, &none).context_window,
        Some(32_768)
    );
    assert_eq!(
        resolve("ollama/qwen3-coder:30b", true, &none).context_window,
        Some(262_144)
    );
}

#[test]
fn keys_match_without_regard_to_case_and_across_slashes() {
    let none = BTreeMap::new();
    assert_eq!(
        resolve("lmstudio/Qwen/Qwen3-Coder-30B-A3B-Instruct", true, &none).context_window,
        Some(262_144)
    );
    assert_eq!(
        resolve("openrouter/anthropic/claude-sonnet-4.5", false, &none).context_window,
        Some(200_000)
    );
}

#[test]
fn defaults_depend_on_whether_the_model_is_local() {
    let none = BTreeMap::new();
    let local = resolve("ollama/some-new-model", true, &none);
    assert_eq!(
        local,
        ModelProfile {
            context_window: None,
            min_context: DEFAULT_MIN_CONTEXT,
            max_output_tokens: None,
            temperature: None,
            reasoning_effort: None,
            text_tool_calls: true,
            local: true,
        }
    );
    let hosted = resolve("openrouter/some-new-model", false, &none);
    assert!(!hosted.local);
    assert!(!hosted.text_tool_calls);
    // A profile can say otherwise: a hosted open-weight model that writes tool calls as text.
    let mine = user(&[(
        "openrouter/*",
        ProfileSettings {
            text_tool_calls: Some(true),
            ..ProfileSettings::default()
        },
    )]);
    assert!(resolve("openrouter/some-new-model", false, &mine).text_tool_calls);
}

#[test]
fn hosted_model_families_know_their_windows() {
    let none = BTreeMap::new();
    for (id, tokens) in [
        ("anthropic/claude-sonnet-4-5", 200_000),
        ("chatgpt/gpt-5.5", 272_000),
        ("openai/gpt-5", 272_000),
        ("openai/gpt-4.1-mini", 1_047_576),
    ] {
        assert_eq!(
            resolve(id, false, &none).context_window,
            Some(tokens),
            "{id}"
        );
    }
}

#[test]
fn every_builtin_key_is_a_valid_glob() {
    for (key, _) in builtin_profiles() {
        assert!(globset::Glob::new(key).is_ok(), "{key} is not a valid glob");
    }
}

#[test]
fn local_means_a_local_server_or_the_loopback_interface() {
    assert!(is_local("ollama/m", "http://gpu-box:11434/v1"));
    assert!(is_local("lmstudio/m", "http://127.0.0.1:1234/v1"));
    assert!(is_local("mine/m", "http://localhost:8000/v1"));
    assert!(is_local("mine/m", "http://127.0.0.2:8000/v1"));
    assert!(is_local("mine/m", "http://[::1]:8000/v1"));
    assert!(!is_local("mine/m", "https://llm.example/v1"));
    assert!(!is_local("openai/gpt-5", "https://api.openai.com/v1"));
}

#[test]
fn a_profile_gives_the_request_options() {
    let mine = user(&[(
        "openai/*",
        ProfileSettings {
            max_output_tokens: Some(8_000),
            temperature: Some(0.3),
            reasoning_effort: Some("high".into()),
            ..ProfileSettings::default()
        },
    )]);
    assert_eq!(
        resolve("openai/gpt-5", false, &mine).request_options(),
        RequestOptions {
            max_output_tokens: Some(8_000),
            temperature: Some(0.3),
            reasoning_effort: Some("high".into()),
        }
    );
}
```

Append to `crates/harness-providers/tests/openai_chat_parser.rs`, after a blank line:

```rust
#[test]
fn profile_options_reach_the_chat_request() {
    use harness_core::message::RequestOptions;
    let plain = ChatRequest {
        model: "m".into(),
        ..ChatRequest::default()
    };
    let body = request_body(&plain);
    for absent in ["max_tokens", "temperature", "reasoning_effort"] {
        assert!(body.get(absent).is_none(), "{absent}");
    }
    let tuned = ChatRequest {
        options: RequestOptions {
            max_output_tokens: Some(2048),
            temperature: Some(0.2),
            reasoning_effort: Some("low".into()),
        },
        ..plain
    };
    let body = request_body(&tuned);
    assert_eq!(body["max_tokens"], 2048);
    assert_eq!(body["temperature"], 0.2);
    assert_eq!(body["reasoning_effort"], "low");
}
```

Create `crates/harness-cli/tests/profiles_e2e.rs`:

```rust
//! Model profiles in `harness ask`: the context window and request settings come from the
//! profile of the model in use.

use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_harness");

fn answer(text: &str) -> ResponseTemplate {
    let chunk =
        json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": "stop"}]});
    ResponseTemplate::new(200).set_body_raw(
        format!("data: {chunk}\n\ndata: [DONE]\n\n"),
        "text/event-stream",
    )
}

struct Env {
    home: TempDir,
    ws: TempDir,
}

impl Env {
    /// Provider `mock` at `server_uri`, and `extra` config (profiles).
    fn new(server_uri: &str, extra: &str) -> Env {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(
            home.path().join("config/config.toml"),
            format!(
                "model = \"mock/coder\"\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{server_uri}/v1\"\n{extra}"
            ),
        )
        .unwrap();
        std::fs::create_dir(ws.path().join(".git")).unwrap();
        Env { home, ws }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.current_dir(self.ws.path())
            .env("HARNESS_HOME", self.home.path())
            .env("HARNESS_CREDENTIAL_STORE", "file")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME");
        cmd
    }
}

async fn body_of_last_request(server: &MockServer) -> Value {
    let requests = server.received_requests().await.unwrap();
    requests.last().unwrap().body_json().unwrap()
}

// Spec: "Unknown context window".
#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_window_is_assumed_small_with_one_warning() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(answer("ok"))
        .mount(&server)
        .await;
    let env = Env::new(&server.uri(), "");
    let output =
        tokio::task::spawn_blocking(move || env.cmd().args(["ask", "hi"]).output().unwrap())
            .await
            .unwrap();
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let warning = "the context window of mock/coder is unknown; assuming 8192 tokens";
    assert_eq!(stderr.matches(warning).count(), 1, "{stderr}");
    assert!(stderr.contains("[profiles.\"mock/coder\"]"), "{stderr}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_profile_sets_the_window_and_the_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(answer("ok"))
        .mount(&server)
        .await;
    let profile = "[profiles.\"mock/*\"]\ncontext_window = 16384\ntemperature = 0.2\nmax_output_tokens = 2048\n";
    let env = Env::new(&server.uri(), profile);
    // About 5,000 tokens of instructions: more than a quarter of 16,384.
    std::fs::write(env.ws.path().join("AGENTS.md"), "x".repeat(20_000)).unwrap();
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["ask", "hi"])
            .assert()
            .success()
            .stderr(contains("the 16384-token context window"))
            .stderr(contains("is unknown").not());
    })
    .await
    .unwrap();
    let body = body_of_last_request(&server).await;
    assert_eq!(body["temperature"], 0.2);
    assert_eq!(body["max_tokens"], 2048);
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p harness-config --test config; cargo test -p harness-providers --test profiles; cargo test -p harness-providers --test openai_chat_parser; cargo test -p harness-cli --test profiles_e2e`
Expected: `config` FAILS to compile (``no field `profiles` on type `Config` ``), and so does `profiles` (``unresolved import `harness_config::config::ProfileSettings` ``, ``unresolved import `harness_providers::profiles` ``, ``cannot find module or crate `globset` ``). `openai_chat_parser` FAILS `profile_options_reach_the_chat_request` (`left: Null, right: 2048`: no `max_tokens`); its other 10 pass. `profiles_e2e` FAILS both: `the_profile_sets_the_window_and_the_request` exits 2 with ``unknown field `profiles` `` in the config, and `an_unknown_window_is_assumed_small_with_one_warning` finds no warning (count 0, not 1).

- [ ] **Step 3: Read profiles in the configuration**

A glob is checked when the file is read, so a typo in a key is an error naming the file. In `crates/harness-config/Cargo.toml`:

Replace:

```toml
publish.workspace = true

[dependencies]
harness-core.workspace = true
hex.workspace = true
serde.workspace = true
```

with:

```toml
publish.workspace = true

[dependencies]
globset.workspace = true
harness-core.workspace = true
hex.workspace = true
serde.workspace = true
```

In `crates/harness-config/src/config.rs`:

Replace (1 of 9):

```rust
    }
}

/// One `config.toml` file as written by the user.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    pub model: Option<String>,
```

with:

```rust
    }
}

/// `[profiles."<glob>"]`: settings for the models whose ids match the glob (resolved in
/// `harness_providers::profiles`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileSettings {
    /// The model's context window, in tokens.
    pub context_window: Option<u64>,
    /// The smallest window worth running agentic turns in; below it harness warns.
    pub min_context: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub reasoning_effort: Option<String>,
    /// Whether tool calls the model writes as text are run.
    pub text_tool_calls: Option<bool>,
    /// Whether the model runs on a server of the user's own.
    pub local: Option<bool>,
}

impl ProfileSettings {
    /// These settings with `other`'s over them.
    pub fn overlaid(&self, other: &ProfileSettings) -> ProfileSettings {
        ProfileSettings {
            context_window: other.context_window.or(self.context_window),
            min_context: other.min_context.or(self.min_context),
            max_output_tokens: other.max_output_tokens.or(self.max_output_tokens),
            temperature: other.temperature.or(self.temperature),
            reasoning_effort: other
                .reasoning_effort
                .clone()
                .or_else(|| self.reasoning_effort.clone()),
            text_tool_calls: other.text_tool_calls.or(self.text_tool_calls),
            local: other.local.or(self.local),
        }
    }

    /// The settings that are set, as `key = value`, for listings and fingerprints.
    fn describe(&self) -> String {
        let mut set = Vec::new();
        let mut number = |key: &str, value: Option<u64>| {
            if let Some(value) = value {
                set.push(format!("{key} = {value}"));
            }
        };
        number("context_window", self.context_window);
        number("min_context", self.min_context);
        number("max_output_tokens", self.max_output_tokens);
        if let Some(t) = self.temperature {
            set.push(format!("temperature = {t}"));
        }
        if let Some(effort) = &self.reasoning_effort {
            set.push(format!("reasoning_effort = {effort:?}"));
        }
        if let Some(on) = self.text_tool_calls {
            set.push(format!("text_tool_calls = {on}"));
        }
        if let Some(local) = self.local {
            set.push(format!("local = {local}"));
        }
        set.join(", ")
    }

    /// What is wrong with the profile under `key`, if anything.
    fn problem(&self, key: &str) -> Option<String> {
        if let Err(e) = globset::Glob::new(key) {
            return Some(format!("profiles.{key:?} is not a valid glob: {e}"));
        }
        for (name, value) in [
            ("context_window", self.context_window),
            ("min_context", self.min_context),
            ("max_output_tokens", self.max_output_tokens),
        ] {
            if value == Some(0) {
                return Some(format!("profiles.{key:?}: {name} must be at least 1"));
            }
        }
        if self.temperature.is_some_and(|t| !(0.0..=2.0).contains(&t)) {
            return Some(format!(
                "profiles.{key:?}: temperature must be between 0 and 2"
            ));
        }
        None
    }
}

/// The first problem with any of `profiles`.
fn profiles_problem(profiles: &BTreeMap<String, ProfileSettings>) -> Option<String> {
    profiles
        .iter()
        .find_map(|(key, profile)| profile.problem(key))
}

/// One `config.toml` file as written by the user.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    pub model: Option<String>,
```

Replace (2 of 9):

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
    pub profiles: BTreeMap<String, ProfileSettings>,
}

#[derive(Debug, thiserror::Error)]
```

Replace (3 of 9):

```rust
}

/// The merged, effective configuration plus warnings about settings that were ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub model: Option<String>,
    pub mode: Option<Mode>,
```

with:

```rust
}

/// The merged, effective configuration plus warnings about settings that were ignored.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    pub model: Option<String>,
    pub mode: Option<Mode>,
```

Replace (4 of 9):

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
    /// Model profiles by model-id glob: the global config's, with a trusted project's over them.
    pub profiles: BTreeMap<String, ProfileSettings>,
    /// Whether the user trusted this workspace with its project settings as they are now
    /// (`harness trust`), so that their widening settings apply. A workspace with no such
    /// settings can be trusted too. A project command file's `model` applies only then.
```

Replace (5 of 9):

```rust
    if let Some(true) = project.sandbox.allow_localhost {
        items.push("sandbox.allow_localhost = true".to_string());
    }
    if project.sandbox.linux_git_protection == Some(LinuxGitProtection::BestEffort)
        && baseline.linux_git_protection == LinuxGitProtection::Required
    {
```

with:

```rust
    if let Some(true) = project.sandbox.allow_localhost {
        items.push("sandbox.allow_localhost = true".to_string());
    }
    // They choose output limits, reasoning effort and context budgets (paid requests), and
    // whether text is run as tool calls.
    for (key, profile) in &project.profiles {
        items.push(format!("profiles.{key:?}: {}", profile.describe()));
    }
    if project.sandbox.linux_git_protection == Some(LinuxGitProtection::BestEffort)
        && baseline.linux_git_protection == LinuxGitProtection::Required
    {
```

Replace (6 of 9):

```rust
        .compaction
        .out_of_range()
        .or_else(|| global_compaction.overlaid(&project.compaction).problem())
    {
        return Err(ConfigError::Parse { path, message });
    }
```

with:

```rust
        .compaction
        .out_of_range()
        .or_else(|| global_compaction.overlaid(&project.compaction).problem())
        .or_else(|| profiles_problem(&project.profiles))
    {
        return Err(ConfigError::Parse { path, message });
    }
```

Replace (7 of 9):

```rust
    let mut cfg = Config::default();
    let global = parse_file(global_file)?;
    let baseline = Baseline::new(global.as_ref(), workspace);
    if let Some(message) = global.as_ref().and_then(|g| g.compaction.problem()) {
        return Err(ConfigError::Parse {
            path: global_file.to_path_buf(),
            message,
```

with:

```rust
    let mut cfg = Config::default();
    let global = parse_file(global_file)?;
    let baseline = Baseline::new(global.as_ref(), workspace);
    if let Some(message) = global.as_ref().and_then(|g| {
        g.compaction
            .problem()
            .or_else(|| profiles_problem(&g.profiles))
    }) {
        return Err(ConfigError::Parse {
            path: global_file.to_path_buf(),
            message,
```

Replace (8 of 9):

```rust
        cfg.writable_roots = expand_all(&global.sandbox.writable_roots, base, home);
        cfg.allow_localhost = global.sandbox.allow_localhost.unwrap_or(false);
        cfg.linux_git_protection = global.sandbox.linux_git_protection.unwrap_or_default();
    }
    let path = project_file(workspace);
    let project = parse_file(&path)?;
    let widening = widening(project.as_ref().unwrap_or(&ConfigFile::default()), baseline);
    cfg.trusted = trust.is_trusted(workspace, &widening.fingerprint);
    if let Some(project) = project {
        if let Some(message) = project.compaction.out_of_range() {
            return Err(ConfigError::Parse { path, message });
        }
        cfg.deny.extend(project.permissions.deny.iter().cloned());
```

with:

```rust
        cfg.writable_roots = expand_all(&global.sandbox.writable_roots, base, home);
        cfg.allow_localhost = global.sandbox.allow_localhost.unwrap_or(false);
        cfg.linux_git_protection = global.sandbox.linux_git_protection.unwrap_or_default();
        cfg.profiles = global.profiles;
    }
    let path = project_file(workspace);
    let project = parse_file(&path)?;
    let widening = widening(project.as_ref().unwrap_or(&ConfigFile::default()), baseline);
    cfg.trusted = trust.is_trusted(workspace, &widening.fingerprint);
    if let Some(project) = project {
        if let Some(message) = project
            .compaction
            .out_of_range()
            .or_else(|| profiles_problem(&project.profiles))
        {
            return Err(ConfigError::Parse { path, message });
        }
        cfg.deny.extend(project.permissions.deny.iter().cloned());
```

Replace (9 of 9):

```rust
                if let Some(protection) = project.sandbox.linux_git_protection {
                    cfg.linux_git_protection = protection;
                }
            } else {
                let items: Vec<&str> = widening_items.iter().map(|item| item.as_str()).collect();
                cfg.warnings.push(format!(
```

with:

```rust
                if let Some(protection) = project.sandbox.linux_git_protection {
                    cfg.linux_git_protection = protection;
                }
                for (key, profile) in &project.profiles {
                    let merged = cfg.profiles.entry(key.clone()).or_default();
                    *merged = merged.overlaid(profile);
                }
            } else {
                let items: Vec<&str> = widening_items.iter().map(|item| item.as_str()).collect();
                cfg.warnings.push(format!(
```

- [ ] **Step 4: Resolve a model's profile**

In `crates/harness-providers/Cargo.toml`:

Replace:

```toml
base64 = { workspace = true, optional = true }
eventsource-stream.workspace = true
futures.workspace = true
harness-config.workspace = true
harness-core.workspace = true
keyring-core.workspace = true
```

with:

```toml
base64 = { workspace = true, optional = true }
eventsource-stream.workspace = true
futures.workspace = true
globset.workspace = true
harness-config.workspace = true
harness-core.workspace = true
keyring-core.workspace = true
```

Create `crates/harness-providers/src/profiles.rs`:

```rust
//! Model profiles: per-model settings from the user's configuration, then the profiles built into
//! harness, then protocol defaults. Keys are globs over model ids, matched without regard to
//! case, and each setting is resolved on its own; within a layer the matching key with the most
//! characters other than `*` and `?` wins.

use std::collections::BTreeMap;

use globset::GlobBuilder;
use harness_config::config::ProfileSettings;
use harness_core::message::RequestOptions;

use crate::registry::LOCAL_PROVIDERS;

/// The smallest context window worth running agentic turns in, unless a profile says otherwise.
pub const DEFAULT_MIN_CONTEXT: u64 = 32_768;
/// The context window assumed when neither the server nor a profile gives one.
pub const FALLBACK_CONTEXT_WINDOW: u64 = 8_192;

/// The settings that apply to one model.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelProfile {
    /// `None` when no profile knows it.
    pub context_window: Option<u64>,
    pub min_context: u64,
    pub max_output_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub reasoning_effort: Option<String>,
    pub text_tool_calls: bool,
    pub local: bool,
}

impl ModelProfile {
    /// What every request to the model carries.
    pub fn request_options(&self) -> RequestOptions {
        RequestOptions {
            max_output_tokens: self.max_output_tokens,
            temperature: self.temperature,
            reasoning_effort: self.reasoning_effort.clone(),
        }
    }
}

fn window(tokens: u64) -> ProfileSettings {
    ProfileSettings {
        context_window: Some(tokens),
        ..ProfileSettings::default()
    }
}

/// The profiles shipped in harness: common open-weight coding families, from their model cards,
/// and the hosted providers' families.
pub fn builtin_profiles() -> Vec<(&'static str, ProfileSettings)> {
    vec![
        // Qwen3-Coder: 256K tokens; Qwen recommends temperature 0.7 (with top_p 0.8, top_k 20).
        (
            "*/qwen3-coder*",
            ProfileSettings {
                temperature: Some(0.7),
                ..window(262_144)
            },
        ),
        // Qwen3: 32,768 tokens natively.
        ("*/qwen3*", window(32_768)),
        // Qwen2.5-Coder: 32,768 tokens without YaRN.
        ("*/qwen2.5-coder*", window(32_768)),
        // Devstral: 128K tokens.
        ("*/devstral*", window(131_072)),
        // gpt-oss: 128K tokens; OpenAI recommends temperature 1.0.
        (
            "*/gpt-oss*",
            ProfileSettings {
                temperature: Some(1.0),
                ..window(131_072)
            },
        ),
        // GLM-4.5: 128K tokens.
        ("*/glm-4.5*", window(131_072)),
        // DeepSeek-Coder-V2: 128K tokens.
        ("*/deepseek-coder-v2*", window(131_072)),
        // Llama 3.1: 128K tokens.
        ("*/llama3.1*", window(131_072)),
        ("*/llama-3.1*", window(131_072)),
        // Claude: 200K tokens.
        ("anthropic/*", window(200_000)),
        ("*/claude-*", window(200_000)),
        // ChatGPT's models, as Codex lists them (codex-rs/models-manager/models.json), and the
        // GPT-5 family's input limit on the API.
        ("chatgpt/*", window(272_000)),
        ("openai/gpt-5*", window(272_000)),
        ("openai/gpt-4.1*", window(1_047_576)),
        ("openai/gpt-4o*", window(128_000)),
        ("openai/o3*", window(200_000)),
        ("openai/o4-mini*", window(200_000)),
    ]
}

/// The profile of `model_id`. `local` says whether its provider is a local server (see
/// [`is_local`]); a profile may say otherwise.
pub fn resolve(
    model_id: &str,
    local: bool,
    user: &BTreeMap<String, ProfileSettings>,
) -> ModelProfile {
    let builtin = builtin_profiles();
    let layers: Vec<&ProfileSettings> =
        matching(user.iter().map(|(k, v)| (k.as_str(), v)), model_id)
            .into_iter()
            .chain(matching(builtin.iter().map(|(k, v)| (*k, v)), model_id))
            .collect();
    let local = layers.iter().find_map(|p| p.local).unwrap_or(local);
    ModelProfile {
        context_window: layers.iter().find_map(|p| p.context_window),
        min_context: layers
            .iter()
            .find_map(|p| p.min_context)
            .unwrap_or(DEFAULT_MIN_CONTEXT),
        max_output_tokens: layers.iter().find_map(|p| p.max_output_tokens),
        temperature: layers.iter().find_map(|p| p.temperature),
        reasoning_effort: layers.iter().find_map(|p| p.reasoning_effort.clone()),
        text_tool_calls: layers
            .iter()
            .find_map(|p| p.text_tool_calls)
            .unwrap_or(local),
        local,
    }
}

/// The profiles whose keys match `model_id`, the most specific first.
fn matching<'a>(
    profiles: impl Iterator<Item = (&'a str, &'a ProfileSettings)>,
    model_id: &str,
) -> Vec<&'a ProfileSettings> {
    let mut found: Vec<(usize, &ProfileSettings)> = profiles
        .filter(|(key, _)| matches(key, model_id))
        .map(|(key, profile)| {
            (
                key.chars().filter(|c| !matches!(c, '*' | '?')).count(),
                profile,
            )
        })
        .collect();
    // Stable: equally specific keys keep their order.
    found.sort_by_key(|(specificity, _)| std::cmp::Reverse(*specificity));
    found.into_iter().map(|(_, profile)| profile).collect()
}

fn matches(key: &str, model_id: &str) -> bool {
    GlobBuilder::new(key)
        .case_insensitive(true)
        .build()
        .is_ok_and(|glob| glob.compile_matcher().is_match(model_id))
}

/// Whether `model_id` runs on a server of the user's own: one of the local servers harness knows
/// (wherever it runs), or a server on the loopback interface.
pub fn is_local(model_id: &str, base_url: &str) -> bool {
    let provider = model_id.split('/').next().unwrap_or_default();
    if LOCAL_PROVIDERS.contains(&provider) {
        return true;
    }
    let Some(host) = reqwest::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(String::from))
    else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}
```

In `crates/harness-providers/src/lib.rs`:

Replace:

```rust
pub mod discovery;
pub mod openai_chat;
pub mod openai_responses;
pub mod registry;
mod sse;
```

with:

```rust
pub mod discovery;
pub mod openai_chat;
pub mod openai_responses;
pub mod profiles;
pub mod registry;
mod sse;
```

In `crates/harness-providers/src/registry.rs`:

Replace:

```rust
/// The built-in provider a ChatGPT account answers for.
pub const CHATGPT: &str = "chatgpt";

const LOCAL_PROVIDERS: [&str; 3] = ["ollama", "lmstudio", "llamacpp"];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
```

with:

```rust
/// The built-in provider a ChatGPT account answers for.
pub const CHATGPT: &str = "chatgpt";

/// The local model servers harness finds on its own.
pub const LOCAL_PROVIDERS: [&str; 3] = ["ollama", "lmstudio", "llamacpp"];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
```

In `crates/harness-providers/src/openai_chat.rs`:

Replace:

```rust
        "stream_options": {"include_usage": true},
        "messages": messages,
    });
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(
            req.tools
```

with:

```rust
        "stream_options": {"include_usage": true},
        "messages": messages,
    });
    // `max_tokens`, which local servers read, rather than OpenAI's newer name.
    if let Some(tokens) = req.options.max_output_tokens {
        body["max_tokens"] = json!(tokens);
    }
    if let Some(temperature) = req.options.temperature {
        body["temperature"] = json!(temperature);
    }
    if let Some(effort) = &req.options.reasoning_effort {
        body["reasoning_effort"] = json!(effort);
    }
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(
            req.tools
```

- [ ] **Step 5: Use the profile in `harness ask`**

In `crates/harness-cli/src/ask.rs`:

Replace (1 of 3):

```rust
    permission::{FsAccess, Mode},
    tool::{CommandSandbox, ToolContext},
};
use harness_providers::registry;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

```

with:

```rust
    permission::{FsAccess, Mode},
    tool::{CommandSandbox, ToolContext},
};
use harness_providers::{profiles, registry};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

```

Replace (2 of 3):

```rust
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
```

with:

```rust
        );
    }
    let ctx = tool_context(&setup.workspace, sandbox, mode.fs_access()).await;
    let local = profiles::is_local(&resolved.id, &resolved.base_url);
    let profile = profiles::resolve(&resolved.id, local, &setup.config.profiles);
    let context_window = context_window(&resolved.id, &profile);
    let mut config = AgentConfig::new(
        resolved.id.clone(),
        resolved.model.clone(),
        crate::context::system_prompt(
            &setup,
            &prompt::base_prompt(mode, sandboxed),
            context_window,
        ),
        output_dir,
    );
    config.context_window = context_window;
    config.request = profile.request_options();
    if let Some(steps) = setup.config.max_steps {
        config.max_steps = steps;
    }
```

Replace (3 of 3):

```rust
    exit_code(reason, blocked)
}

/// Ends the run: first the agent, which releases the session file, then the sandbox's session,
/// which can take a few seconds, so a `-c` started once the answer prints finds the file free.
fn end_run(agent: Agent, sandbox_session: SessionEnd) {
```

with:

```rust
    exit_code(reason, blocked)
}

/// The context window of model `id`, from its profile; when no profile knows it, the fallback,
/// with a warning that says how to set it.
fn context_window(id: &str, profile: &profiles::ModelProfile) -> u64 {
    profile.context_window.unwrap_or_else(|| {
        eprintln!(
            "warning: the context window of {} is unknown; assuming {} tokens. Set it with `context_window` under [profiles.\"{}\"] in config.toml",
            terminal_safe(id),
            profiles::FALLBACK_CONTEXT_WINDOW,
            terminal_safe(id)
        );
        profiles::FALLBACK_CONTEXT_WINDOW
    })
}

/// Ends the run: first the agent, which releases the session file, then the sandbox's session,
/// which can take a few seconds, so a `-c` started once the answer prints finds the file free.
fn end_run(agent: Agent, sandbox_session: SessionEnd) {
```

In `crates/harness-cli/src/context.rs`:

Replace (1 of 2):

```rust
use std::path::PathBuf;

use harness_context::{environment, instructions, prompt};
use harness_core::{time::today_utc, tokens::DEFAULT_CONTEXT_WINDOW};

use crate::{setup::Setup, term::terminal_safe};

```

with:

```rust
use std::path::PathBuf;

use harness_context::{environment, instructions, prompt};
use harness_core::time::today_utc;

use crate::{setup::Setup, term::terminal_safe};

```

Replace (2 of 2):

```rust

/// Builds the system prompt for a session in `setup.workspace` from `base` (see
/// `prompt::base_prompt`), printing a warning for each instruction file or import that could not
/// be used, and when the instruction files are large.
pub fn system_prompt(setup: &Setup, base: &str) -> String {
    let loaded =
        instructions::discover(&setup.workspace, &setup.paths.config_dir, home().as_deref());
    for warning in &loaded.warnings {
        eprintln!("warning: {}", terminal_safe(warning));
    }
    if let Some(warning) = prompt::oversize_warning(&loaded.files, DEFAULT_CONTEXT_WINDOW) {
        eprintln!("warning: {}", terminal_safe(&warning));
    }
    let environment = environment::capture(&setup.workspace, &today_utc());
```

with:

```rust

/// Builds the system prompt for a session in `setup.workspace` from `base` (see
/// `prompt::base_prompt`), printing a warning for each instruction file or import that could not
/// be used, and when the instruction files take more than a quarter of `context_window`.
pub fn system_prompt(setup: &Setup, base: &str, context_window: u64) -> String {
    let loaded =
        instructions::discover(&setup.workspace, &setup.paths.config_dir, home().as_deref());
    for warning in &loaded.warnings {
        eprintln!("warning: {}", terminal_safe(warning));
    }
    if let Some(warning) = prompt::oversize_warning(&loaded.files, context_window) {
        eprintln!("warning: {}", terminal_safe(&warning));
    }
    let environment = environment::capture(&setup.workspace, &today_utc());
```

In `crates/harness-core/src/tokens.rs`:

Replace:

```rust
//! Rough token counts. harness has no tokenizer for most models, so it counts about four bytes
//! per token, which is close for English text and code.

/// The context window assumed for every model until model profiles report the real one.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 32_768;

/// Estimated tokens in `text`: its length in bytes divided by four, rounded up.
```

with:

```rust
//! Rough token counts. harness has no tokenizer for most models, so it counts about four bytes
//! per token, which is close for English text and code.

/// The context window an agent starts with; `harness ask` sets the model's own, from its profile
/// and its server.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 32_768;

/// Estimated tokens in `text`: its length in bytes divided by four, rounded up.
```

- [ ] **Step 6: Give P3's window-sensitive e2e tests their window**

In `crates/harness-cli/tests/compaction_e2e.rs`:

Replace:

```rust
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(
            home.path().join("config/config.toml"),
            format!("model = \"mock/test-model\"\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{server_uri}/v1\"\n"),
        )
        .unwrap();
        std::fs::create_dir(ws.path().join(".git")).unwrap();
```

with:

```rust
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(
            home.path().join("config/config.toml"),
            // These tests measure against a 32,768-token window.
            format!("model = \"mock/test-model\"\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{server_uri}/v1\"\n[profiles.\"mock/*\"]\ncontext_window = 32768\n"),
        )
        .unwrap();
        std::fs::create_dir(ws.path().join(".git")).unwrap();
```

In `crates/harness-cli/tests/context_e2e.rs`:

Replace:

```rust
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(
            home.path().join("config/config.toml"),
            format!("model = \"mock/test-model\"\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{server_uri}/v1\"\n"),
        )
        .unwrap();
        std::fs::create_dir(ws.path().join(".git")).unwrap();
```

with:

```rust
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(
            home.path().join("config/config.toml"),
            // These tests measure against a 32,768-token window.
            format!("model = \"mock/test-model\"\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{server_uri}/v1\"\n[profiles.\"mock/*\"]\ncontext_window = 32768\n"),
        )
        .unwrap();
        std::fs::create_dir(ws.path().join(".git")).unwrap();
```

In `crates/harness-cli/tests/sandbox_e2e.rs`:

Replace:

```rust
        std::fs::create_dir_all(env.home.path().join("config")).unwrap();
        std::fs::write(
            env.home.path().join("config/config.toml"),
            format!("model = \"mock/m\"\n{extra_config}\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{server_uri}/v1\"\n"),
        )
        .unwrap();
        let git = StdCommand::new("git")
```

with:

```rust
        std::fs::create_dir_all(env.home.path().join("config")).unwrap();
        std::fs::write(
            env.home.path().join("config/config.toml"),
            // A known window, so that the only warnings are the sandbox's.
            format!("model = \"mock/m\"\n{extra_config}\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{server_uri}/v1\"\n[profiles.\"mock/*\"]\ncontext_window = 32768\n"),
        )
        .unwrap();
        let git = StdCommand::new("git")
```

- [ ] **Step 7: Run the tests**

Run: `cargo test -p harness-config -p harness-providers -p harness-cli`
Expected: PASS: 37 in `config`, 8 in `profiles`, 11 in `openai_chat_parser` and 2 in `profiles_e2e`, with the adjusted `compaction_e2e`, `context_e2e` and `sandbox_e2e`; 284 tests in the three crates.

- [ ] **Step 8: Lint**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo clippy -p harness-cli --no-default-features --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 9: Commit**

```bash
git add Cargo.lock crates/harness-config crates/harness-core crates/harness-providers crates/harness-cli
git commit -F - <<'EOF'
feat(providers): resolve model profiles

[profiles."<glob>"] sets a model's context window, minimum context,
output limit, temperature, reasoning effort, text tool calls and
whether it is local. Each setting comes from the user's profiles, then
built-in ones for common open-weight coding families and the hosted
families, then defaults; the most specific key wins, and keys match in
any case. A project's profiles need workspace trust. harness ask takes
the window and request settings from the profile, warns once when no
profile knows the window and assumes 8,192 tokens, and measures the
instruction files against the real window. The chat adapter sends the
output limit, temperature and reasoning effort.

<trailer lines from the controller>
EOF
```

---

### Task 8: The window local servers really run

**Files:**
- Create: `crates/harness-providers/src/window.rs`, `crates/harness-providers/tests/window.rs`, `crates/harness-cli/tests/local_e2e.rs`
- Modify: `crates/harness-providers/src/lib.rs`, `crates/harness-cli/src/ask.rs`

**Interfaces:**
- Consumes: `profiles::{ModelProfile, FALLBACK_CONTEXT_WINDOW, is_local, resolve}` (Task 7).
- Produces:
  - `harness_providers::window::{PROBE_TIMEOUT (1 s), LOAD_TIMEOUT (120 s), Server { Ollama, LlamaCpp, LmStudio }, running_context, Window { tokens: u64, warnings: Vec<String> }, effective_window}`: `Server::of(provider, &providers) -> Option<Server>`, `running_context(server, base_url, model, probe: Duration, load: Duration) -> Option<u64>` (async), `effective_window(id, &ModelProfile, running: Option<u64>, server: Option<Server>) -> Window`.
  - In the CLI: `ask` asks the server, warns, and uses `Window::tokens`. The unknown-window warning moves from Task 7's helper into `effective_window`, with the same words. Continuing a local conversation on a hosted model says nothing (decision 14, the maintainer's later choice); there is no `sessions::held_only_locally`.

- [ ] **Step 1: Write the failing tests**

Create `crates/harness-providers/tests/window.rs`:

```rust
//! The context window a local server really runs a model with, and the window harness uses.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use harness_config::config::{Protocol, ProviderConfig};
use harness_providers::profiles::{self, FALLBACK_CONTEXT_WINDOW};
use harness_providers::window::{Server, effective_window, running_context};
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PROBE: Duration = Duration::from_millis(500);
const LOAD: Duration = Duration::from_secs(5);

fn base(server: &MockServer) -> String {
    format!("{}/v1", server.uri())
}

fn ps(models: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "models": models }))
}

#[tokio::test]
async fn ollama_reports_a_loaded_models_context() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/ps"))
        .respond_with(ps(json!([
            {"name": "llama3.1:latest", "model": "llama3.1:latest", "context_length": 131072},
            {"name": "qwen3-coder:30b", "model": "qwen3-coder:30b", "context_length": 4096}
        ])))
        .mount(&server)
        .await;
    let found = |model: &'static str| {
        let base = base(&server);
        async move { running_context(Server::Ollama, &base, model, PROBE, LOAD).await }
    };
    assert_eq!(found("qwen3-coder:30b").await, Some(4096));
    // A name without a tag is Ollama's `latest`.
    assert_eq!(found("llama3.1").await, Some(131_072));
}

#[tokio::test]
async fn ollama_loads_a_model_that_is_not_running_yet() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/ps"))
        .respond_with(ps(json!([])))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/generate"))
        .and(body_string_contains("qwen3-coder:30b"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"done": true})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/ps"))
        .respond_with(ps(
            json!([{"name": "qwen3-coder:30b", "context_length": 8192}]),
        ))
        .with_priority(2)
        .mount(&server)
        .await;
    assert_eq!(
        running_context(
            Server::Ollama,
            &base(&server),
            "qwen3-coder:30b",
            PROBE,
            LOAD
        )
        .await,
        Some(8192)
    );
}

#[tokio::test]
async fn llama_cpp_reports_its_slot_context() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .and(query_param("model", "qwen"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "default_generation_settings": {"id": 0, "n_ctx": 16384, "params": {}},
            "total_slots": 1
        })))
        .mount(&server)
        .await;
    assert_eq!(
        running_context(Server::LlamaCpp, &base(&server), "qwen", PROBE, LOAD).await,
        Some(16_384)
    );
}

#[tokio::test]
async fn lm_studio_reports_a_loaded_instance_only() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": [
            {"key": "qwen/qwen3-coder-30b", "loaded_instances": [
                {"id": "qwen/qwen3-coder-30b", "config": {"context_length": 4096}}
            ], "max_context_length": 262144},
            {"key": "google/gemma-3-12b", "loaded_instances": [], "max_context_length": 131072}
        ]})))
        .mount(&server)
        .await;
    let base = base(&server);
    assert_eq!(
        running_context(Server::LmStudio, &base, "qwen/qwen3-coder-30b", PROBE, LOAD).await,
        Some(4096)
    );
    assert_eq!(
        running_context(Server::LmStudio, &base, "google/gemma-3-12b", PROBE, LOAD).await,
        None
    );
}

// Review Focus: a server that is not running, or does not answer, costs no more than the probe's
// time limit.
#[tokio::test]
async fn a_server_that_does_not_answer_reports_nothing_in_time() {
    let started = Instant::now();
    assert_eq!(
        running_context(Server::LlamaCpp, "http://127.0.0.1:9/v1", "m", PROBE, LOAD).await,
        None
    );
    let slow = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&slow)
        .await;
    assert_eq!(
        running_context(Server::LlamaCpp, &base(&slow), "m", PROBE, LOAD).await,
        None
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

// Spec: "Ollama running with a small context".
#[test]
fn a_small_running_context_wins_and_is_warned_about() {
    let profile = profiles::resolve("ollama/qwen3-coder:30b", true, &BTreeMap::new());
    let window = effective_window(
        "ollama/qwen3-coder:30b",
        &profile,
        Some(4096),
        Some(Server::Ollama),
    );
    assert_eq!(window.tokens, 4096);
    assert_eq!(window.warnings.len(), 1, "{:?}", window.warnings);
    let warning = &window.warnings[0];
    assert!(warning.contains("4096-token context window"), "{warning}");
    assert!(warning.contains("OLLAMA_CONTEXT_LENGTH=32768"), "{warning}");
}

#[test]
fn the_smaller_of_server_and_profile_is_used() {
    let profile = profiles::resolve("llamacpp/qwen3-coder", true, &BTreeMap::new());
    let bigger = effective_window(
        "llamacpp/qwen3-coder",
        &profile,
        Some(1_000_000),
        Some(Server::LlamaCpp),
    );
    assert_eq!(bigger.tokens, 262_144);
    assert!(bigger.warnings.is_empty(), "{:?}", bigger.warnings);
    let smaller = effective_window(
        "llamacpp/qwen3-coder",
        &profile,
        Some(16_384),
        Some(Server::LlamaCpp),
    );
    assert_eq!(smaller.tokens, 16_384);
    assert!(
        smaller.warnings[0].contains("llama-server -c 32768"),
        "{:?}",
        smaller.warnings
    );
}

// Spec: "Unknown context window".
#[test]
fn an_unknown_window_falls_back_with_one_warning() {
    let profile = profiles::resolve("mine/new-model", true, &BTreeMap::new());
    let window = effective_window("mine/new-model", &profile, None, None);
    assert_eq!(window.tokens, FALLBACK_CONTEXT_WINDOW);
    assert_eq!(window.warnings.len(), 1, "{:?}", window.warnings);
    assert!(
        window.warnings[0].contains("unknown; assuming 8192 tokens"),
        "{:?}",
        window.warnings
    );
}

#[test]
fn a_profile_below_its_minimum_is_warned_about_without_a_server() {
    let mine = BTreeMap::from([(
        "mine/*".to_string(),
        harness_config::config::ProfileSettings {
            context_window: Some(8_192),
            ..Default::default()
        },
    )]);
    let profile = profiles::resolve("mine/model", false, &mine);
    let window = effective_window("mine/model", &profile, None, None);
    assert_eq!(window.tokens, 8_192);
    assert_eq!(window.warnings.len(), 1, "{:?}", window.warnings);
    assert!(
        window.warnings[0].contains("context_window"),
        "{:?}",
        window.warnings
    );
}

#[test]
fn the_local_servers_are_known_by_name() {
    let mut providers = BTreeMap::new();
    assert_eq!(Server::of("ollama", &providers), Some(Server::Ollama));
    assert_eq!(Server::of("lmstudio", &providers), Some(Server::LmStudio));
    assert_eq!(Server::of("llamacpp", &providers), Some(Server::LlamaCpp));
    assert_eq!(Server::of("openrouter", &providers), None);
    // Moved to another machine, Ollama is still Ollama; spoken to in another protocol, it is not
    // asked.
    providers.insert(
        "ollama".to_string(),
        ProviderConfig {
            protocol: Protocol::OpenaiChat,
            base_url: "http://gpu-box:11434/v1".into(),
            api_key_env: None,
        },
    );
    assert_eq!(Server::of("ollama", &providers), Some(Server::Ollama));
    providers.get_mut("ollama").unwrap().protocol = Protocol::AnthropicMessages;
    assert_eq!(Server::of("ollama", &providers), None);
}
```

Create `crates/harness-cli/tests/local_e2e.rs`:

```rust
//! Local models in `harness ask`: the window the server really runs the model with, and a
//! conversation held on local models that is continued on a hosted one (which says nothing).

use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;
use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_harness");

fn answer(text: &str) -> ResponseTemplate {
    let chunk =
        json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": "stop"}]});
    ResponseTemplate::new(200).set_body_raw(
        format!("data: {chunk}\n\ndata: [DONE]\n\n"),
        "text/event-stream",
    )
}

struct Env {
    home: TempDir,
    ws: TempDir,
}

impl Env {
    fn new(config: &str) -> Env {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(home.path().join("config/config.toml"), config).unwrap();
        std::fs::create_dir(ws.path().join(".git")).unwrap();
        Env { home, ws }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.current_dir(self.ws.path())
            .env("HARNESS_HOME", self.home.path())
            .env("HARNESS_CREDENTIAL_STORE", "file")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME");
        cmd
    }
}

// Spec: "Ollama running with a small context".
#[tokio::test(flavor = "multi_thread")]
async fn ollamas_small_running_context_is_used_and_explained() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/ps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": [
            {"name": "qwen3-coder:30b", "model": "qwen3-coder:30b", "context_length": 4096}
        ]})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(answer("ok"))
        .mount(&server)
        .await;
    // Ollama moved to the mock server: still Ollama, so it is asked.
    let env = Env::new(&format!(
        "model = \"ollama/qwen3-coder:30b\"\n[providers.ollama]\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\n",
        server.uri()
    ));
    // About 1,250 tokens: more than a quarter of 4,096, but not of the model's 262,144.
    std::fs::write(env.ws.path().join("AGENTS.md"), "x".repeat(5_000)).unwrap();
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["ask", "hi"])
            .assert()
            .success()
            .stderr(contains("runs with a 4096-token context window"))
            .stderr(contains("OLLAMA_CONTEXT_LENGTH=32768"))
            .stderr(contains("the 4096-token context window"));
    })
    .await
    .unwrap();
}

// Decision 14: continuing a conversation held on local models with a hosted model says nothing
// about it (the behaviour before P4); harness does not flag it.
#[tokio::test(flavor = "multi_thread")]
async fn a_local_conversation_continued_on_a_hosted_model_prints_no_warning() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(answer("ok"))
        .mount(&server)
        .await;
    // `mock` is on the loopback interface, so local; `cloud` is too, but its profile says it is
    // not.
    let env = Env::new(&format!(
        "[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{uri}/v1\"\n[providers.cloud]\nprotocol = \"openai-chat\"\nbase_url = \"{uri}/v1\"\n[profiles.\"mock/*\"]\ncontext_window = 32768\n[profiles.\"cloud/*\"]\ncontext_window = 200000\nlocal = false\n",
        uri = server.uri()
    ));
    let flagged = "ran on local models so far; continuing it on";
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["--model", "mock/small", "ask", "one"])
            .assert()
            .success()
            .stderr(contains(flagged).not());
        env.cmd()
            .args(["-c", "--model", "mock/small", "ask", "two"])
            .assert()
            .success()
            .stderr(contains(flagged).not());
        env.cmd()
            .args(["-c", "--model", "cloud/big", "ask", "three"])
            .assert()
            .success()
            .stderr(contains(flagged).not());
        env.cmd()
            .args(["-c", "--model", "cloud/big", "ask", "four"])
            .assert()
            .success()
            .stderr(contains(flagged).not());
        // A new conversation holds nothing yet.
        env.cmd()
            .args(["--model", "cloud/big", "ask", "five"])
            .assert()
            .success()
            .stderr(contains(flagged).not());
    })
    .await
    .unwrap();
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p harness-providers --test window; cargo test -p harness-cli --test local_e2e`
Expected: `window` FAILS to compile: ``unresolved import `harness_providers::window` ``. `local_e2e` FAILS both: stderr lacks `runs with a 4096-token context window`, and lacks `ran on local models so far; continuing it on cloud/big sends it`.

- [ ] **Step 3: Ask the servers**

Ollama's `/api/ps` lists only loaded models, and a name without a tag is its `latest`. llama.cpp answers `/props` with the context of each of its slots, and in router mode needs the model in the query. LM Studio's v1 REST API lists each loaded instance with its context. A request is built with `reqwest`'s `Url`, which avoids `reqwest`'s `query` feature.

Create `crates/harness-providers/src/window.rs`:

```rust
//! The context window a local server really runs a model with, which can be smaller than the
//! model's own: llama.cpp's `/props`, Ollama's running models, LM Studio's loaded instances. A
//! server that does not answer in time reports nothing. The window harness uses is the smaller of
//! that and the model's profile.

use std::{collections::BTreeMap, time::Duration};

use harness_config::config::{Protocol, ProviderConfig};
use serde_json::{Value, json};

use crate::profiles::{FALLBACK_CONTEXT_WINDOW, ModelProfile};

/// How long each question to a server may take.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
/// How long Ollama may take to load a model it is asked about.
pub const LOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// A local server that reports the context it runs models with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Server {
    Ollama,
    LlamaCpp,
    LmStudio,
}

impl Server {
    /// The server behind provider `provider`: one of the built-in local servers, by name, also
    /// when the user moved it to another address, as long as it speaks the Chat Completions
    /// protocol.
    pub fn of(provider: &str, providers: &BTreeMap<String, ProviderConfig>) -> Option<Server> {
        if providers
            .get(provider)
            .is_some_and(|cfg| cfg.protocol != Protocol::OpenaiChat)
        {
            return None;
        }
        match provider {
            "ollama" => Some(Server::Ollama),
            "llamacpp" => Some(Server::LlamaCpp),
            "lmstudio" => Some(Server::LmStudio),
            _ => None,
        }
    }

    /// How to run the model with at least `tokens` tokens of context on this server.
    fn remedy(self, tokens: u64) -> String {
        match self {
            Server::Ollama => format!(
                "restart Ollama with a larger context, e.g. `OLLAMA_CONTEXT_LENGTH={tokens} ollama serve`"
            ),
            Server::LlamaCpp => format!(
                "start llama-server with a larger context, e.g. `llama-server -c {tokens}` (with --parallel, each slot gets a share)"
            ),
            Server::LmStudio => {
                format!("load the model in LM Studio with a context length of {tokens} or more")
            }
        }
    }
}

/// The context `server`, whose Chat Completions endpoint is `base_url`, runs `model` with, if it
/// says. Ollama is asked to load a model it has not loaded yet, which the first request would do
/// anyway, taking up to `load`.
pub async fn running_context(
    server: Server,
    base_url: &str,
    model: &str,
    probe: Duration,
    load: Duration,
) -> Option<u64> {
    let root = base_url.trim_end_matches('/').trim_end_matches("/v1");
    let client = reqwest::Client::new();
    match server {
        Server::Ollama => {
            if let Some(tokens) = ollama_running(&client, root, model, probe).await {
                return Some(tokens);
            }
            // An empty prompt only loads the model.
            client
                .post(format!("{root}/api/generate"))
                .json(&json!({"model": model}))
                .timeout(load)
                .send()
                .await
                .ok()?;
            ollama_running(&client, root, model, probe).await
        }
        Server::LlamaCpp => {
            let props = get(
                &client,
                &format!("{root}/props"),
                &[("model", model)],
                probe,
            )
            .await?;
            props["default_generation_settings"]["n_ctx"].as_u64()
        }
        Server::LmStudio => {
            let listing = get(&client, &format!("{root}/api/v1/models"), &[], probe).await?;
            listing["models"]
                .as_array()?
                .iter()
                .flat_map(|m| {
                    let key = m["key"].as_str() == Some(model);
                    m["loaded_instances"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(move |i| key || i["id"].as_str() == Some(model))
                })
                .filter_map(|instance| instance["config"]["context_length"].as_u64())
                .min()
        }
    }
}

/// The context of `model` among Ollama's running models. A name without a tag is `:latest`.
async fn ollama_running(
    client: &reqwest::Client,
    root: &str,
    model: &str,
    probe: Duration,
) -> Option<u64> {
    let tagged = if model.contains(':') {
        model.to_string()
    } else {
        format!("{model}:latest")
    };
    let running = get(client, &format!("{root}/api/ps"), &[], probe).await?;
    running["models"].as_array()?.iter().find(|m| {
        [&m["name"], &m["model"]]
            .iter()
            .any(|name| name.as_str().is_some_and(|n| n == model || n == tagged))
    })?["context_length"]
        .as_u64()
}

async fn get(
    client: &reqwest::Client,
    url: &str,
    query: &[(&str, &str)],
    timeout: Duration,
) -> Option<Value> {
    let mut url = reqwest::Url::parse(url).ok()?;
    if !query.is_empty() {
        url.query_pairs_mut().extend_pairs(query);
    }
    let response = client
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    response.json().await.ok()
}

/// The window harness uses, and what to tell the user about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub tokens: u64,
    pub warnings: Vec<String>,
}

/// The window of model `id`: the smaller of what its server runs it with (`running`) and its
/// profile's, or the fallback when neither is known. A window below the profile's minimum is
/// warned about, with the fix for its server.
pub fn effective_window(
    id: &str,
    profile: &ModelProfile,
    running: Option<u64>,
    server: Option<Server>,
) -> Window {
    let mut warnings = Vec::new();
    let tokens = match (running, profile.context_window) {
        (Some(running), Some(own)) => running.min(own),
        (Some(running), None) => running,
        (None, Some(own)) => own,
        (None, None) => {
            warnings.push(format!(
                "the context window of {id} is unknown; assuming {FALLBACK_CONTEXT_WINDOW} tokens. Set it with `context_window` under [profiles.\"{id}\"] in config.toml"
            ));
            return Window {
                tokens: FALLBACK_CONTEXT_WINDOW,
                warnings,
            };
        }
    };
    if tokens < profile.min_context {
        let fix = match (running, server) {
            (Some(running), Some(server)) if running == tokens => {
                server.remedy(profile.min_context)
            }
            _ => "raise `context_window` in its profile if the model takes more, or choose a model with a larger window".to_string(),
        };
        warnings.push(format!(
            "{id} runs with a {tokens}-token context window, below the {} tokens agentic work needs, so long tasks will be compacted often; {fix}",
            profile.min_context
        ));
    }
    Window { tokens, warnings }
}
```

In `crates/harness-providers/src/lib.rs`:

Replace:

```rust
pub mod profiles;
pub mod registry;
mod sse;
```

with:

```rust
pub mod profiles;
pub mod registry;
mod sse;
pub mod window;
```

- [ ] **Step 4: Use the window**

`crates/harness-cli/src/sessions.rs` is not touched by this task. An earlier draft of this plan
added `sessions::held_only_locally` here so `ask` could warn when a local conversation continues
on a hosted model (decision 14); the maintainer chose "say nothing", the behaviour before P4, so
that helper and its call site do not exist.

In `crates/harness-cli/src/ask.rs`, the window replaces Task 7's `context_window` helper, and Ctrl+C while a model loads exits 130:

Replace (1 of 3):

```rust
    permission::{FsAccess, Mode},
    tool::{CommandSandbox, ToolContext},
};
use harness_providers::{profiles, registry};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

```

with:

```rust
    permission::{FsAccess, Mode},
    tool::{CommandSandbox, ToolContext},
};
use harness_providers::{
    profiles, registry,
    window::{self, LOAD_TIMEOUT, PROBE_TIMEOUT},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

```

Replace (2 of 3):

```rust
    let ctx = tool_context(&setup.workspace, sandbox, mode.fs_access()).await;
    let local = profiles::is_local(&resolved.id, &resolved.base_url);
    let profile = profiles::resolve(&resolved.id, local, &setup.config.profiles);
    let context_window = context_window(&resolved.id, &profile);
    let mut config = AgentConfig::new(
        resolved.id.clone(),
        resolved.model.clone(),
```

with:

```rust
    let ctx = tool_context(&setup.workspace, sandbox, mode.fs_access()).await;
    let local = profiles::is_local(&resolved.id, &resolved.base_url);
    let profile = profiles::resolve(&resolved.id, local, &setup.config.profiles);
    // The window the server really runs the model with, when it is a local server that says.
    let provider = resolved.id.split('/').next().unwrap_or_default();
    let server = window::Server::of(provider, &setup.config.providers);
    let running = match server {
        Some(server) => tokio::select! {
            tokens = window::running_context(server, &resolved.base_url, &resolved.model, PROBE_TIMEOUT, LOAD_TIMEOUT) => tokens,
            _ = cancel.cancelled() => return exit_code(TurnEndReason::Interrupted, false),
        },
        None => None,
    };
    let window = window::effective_window(&resolved.id, &profile, running, server);
    for warning in &window.warnings {
        eprintln!("warning: {}", terminal_safe(warning));
    }
    let context_window = window.tokens;
    let mut config = AgentConfig::new(
        resolved.id.clone(),
        resolved.model.clone(),
```

Decision 14 (the maintainer's later choice) says nothing when a local conversation continues on a
hosted model: there is no warning here, no `sessions::held_only_locally`, and no `model_is_local`
helper. Replace (3 of 3) below only drops the superseded `context_window` helper; it adds nothing
in its place.

Replace (3 of 3):

```rust
    exit_code(reason, blocked)
}

/// The context window of model `id`, from its profile; when no profile knows it, the fallback,
/// with a warning that says how to set it.
fn context_window(id: &str, profile: &profiles::ModelProfile) -> u64 {
    profile.context_window.unwrap_or_else(|| {
        eprintln!(
            "warning: the context window of {} is unknown; assuming {} tokens. Set it with `context_window` under [profiles.\"{}\"] in config.toml",
            terminal_safe(id),
            profiles::FALLBACK_CONTEXT_WINDOW,
            terminal_safe(id)
        );
        profiles::FALLBACK_CONTEXT_WINDOW
    })
}

/// Ends the run: first the agent, which releases the session file, then the sandbox's session,
```

with:

```rust
    exit_code(reason, blocked)
}

/// Ends the run: first the agent, which releases the session file, then the sandbox's session,
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p harness-providers -p harness-cli`
Expected: PASS: 10 in `window` (among them `a_server_that_does_not_answer_reports_nothing_in_time`) and 2 in `local_e2e`; 254 tests in the two crates.

- [ ] **Step 6: Lint**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add crates/harness-providers crates/harness-cli
git commit -F - <<'EOF'
feat(providers): detect the context window local servers run with

harness ask asks llama.cpp (/props), Ollama (/api/ps, loading the model
first when it is not running yet) and LM Studio (/api/v1/models) what
context they run the model with, and uses the smaller of that and the
profile's window, for compaction and the instruction-size warning. A
window below the profile's minimum is warned about with the fix for
that server, such as OLLAMA_CONTEXT_LENGTH. A server that does not
answer within a second reports nothing. Continuing a conversation held
on local models with a hosted model says that it now leaves the
machine.

<trailer lines from the controller>
EOF
```

---

### Task 9: Tool calls written as text, and replies cut off

**Files:**
- Create: `crates/harness-core/src/textcalls.rs`, `crates/harness-core/tests/local_replies.rs`
- Modify: `crates/harness-core/src/lib.rs`, `crates/harness-core/src/agent.rs`, `crates/harness-cli/src/ask.rs`, `crates/harness-cli/tests/local_e2e.rs`

**Interfaces:**
- Consumes: `ModelProfile::text_tool_calls` (Task 7).
- Produces:
  - `harness_core::textcalls::recover(text: &str, known: impl Fn(&str) -> bool) -> Option<Vec<ToolCall>>`: the calls `text` consists of, with empty ids, when every one names a tool `known` accepts.
  - `AgentConfig::text_tool_calls: bool` (default `false`; `ask` sets it from the profile), and `agent::{CUT_OFF_CALL, CUT_OFF_REPLY}`, the texts given after a cut-off reply.

The agent's loop now reads `ModelReply::finish`, which P3 recorded but left unread. A recovered call gets a fresh id (`call_h<n>`) through the same `dedupe_call_ids` as native ones.

- [ ] **Step 1: Write the failing tests**

Create `crates/harness-core/tests/local_replies.rs`:

````rust
//! What local models do that hosted ones rarely do: write tool calls as text, and run out of
//! output tokens in the middle of a reply.

mod common;

use std::sync::Arc;

use common::*;
use harness_core::agent::NonInteractive;
use harness_core::event::{AgentEvent, TurnEndReason};
use harness_core::message::{Message, ToolCall};
use harness_core::permission::Mode;
use harness_core::provider::{FinishReason, ProviderEvent};
use harness_core::testing::{MockProvider, Script};
use harness_core::textcalls::recover;
use serde_json::json;

fn known(name: &str) -> bool {
    matches!(name, "read" | "echo")
}

fn call(name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: String::new(),
        name: name.into(),
        arguments: arguments.into(),
    }
}

#[test]
fn tagged_calls_are_recovered_whole() {
    assert_eq!(
        recover(
            r#"<tool_call>{"name":"read","arguments":{"path":"src/lib.rs"}}</tool_call>"#,
            known
        ),
        Some(vec![call("read", r#"{"path":"src/lib.rs"}"#)])
    );
    let two = "\n<tool_call>\n{\"name\": \"echo\", \"arguments\": {\"text\": \"a\"}}\n</tool_call>\n\n<tool_call>{\"name\": \"echo\", \"arguments\": {\"text\": \"b\"}}</tool_call>\n";
    assert_eq!(
        recover(two, known),
        Some(vec![
            call("echo", r#"{"text":"a"}"#),
            call("echo", r#"{"text":"b"}"#)
        ])
    );
}

#[test]
fn a_message_that_is_only_a_call_object_is_recovered() {
    assert_eq!(
        recover(r#" {"name": "read", "arguments": {"path": "a"}} "#, known),
        Some(vec![call("read", r#"{"path":"a"}"#)])
    );
    // Llama 3.1 writes `parameters`.
    assert_eq!(
        recover(r#"{"name": "read", "parameters": {"path": "a"}}"#, known),
        Some(vec![call("read", r#"{"path":"a"}"#)])
    );
    // Arguments already written as JSON text are kept as they are.
    assert_eq!(
        recover(
            r#"{"name": "read", "arguments": "{\"path\": \"a\"}"}"#,
            known
        ),
        Some(vec![call("read", r#"{"path": "a"}"#)])
    );
}

// Spec: "Example code in prose". Review Focus: a reply that explains or quotes the format is not
// a call.
#[test]
fn text_around_a_call_or_a_quoted_call_is_not_recovered() {
    for text in [
        r#"To read a file, send {"name": "read", "arguments": {"path": "a"}} and wait."#,
        "Here is the call:\n<tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"a\"}}</tool_call>",
        "<tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"a\"}}</tool_call>\nDone.",
        "```json\n{\"name\": \"read\", \"arguments\": {\"path\": \"a\"}}\n```",
        "<tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"a\"}}",
        r#"{"name": "read"}"#,
        r#"{"name": "deploy", "arguments": {}}"#,
        r#"[{"name": "read", "arguments": {"path": "a"}}]"#,
        "",
    ] {
        assert_eq!(recover(text, known), None, "{text:?}");
    }
}

fn local_agent(provider: Arc<MockProvider>, dir: &std::path::Path) -> harness_core::agent::Agent {
    let mut agent = agent(provider, Mode::Auto, Arc::new(NonInteractive), dir);
    agent.config_mut().text_tool_calls = true;
    agent
}

// Spec: "Local model emits a tagged tool call as text".
#[tokio::test]
async fn a_tagged_call_in_a_reply_runs_like_a_native_one() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::text(r#"<tool_call>{"name":"echo","arguments":{"text":"ran"}}</tool_call>"#),
        Script::text("done"),
    ]);
    let mut agent = local_agent(provider.clone(), dir.path());
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(finished_outputs(&events), [("ran".to_string(), false)]);
    // The call is kept as a call, so the model sees it in its native form next time.
    match &agent.history()[1] {
        Message::Assistant {
            content,
            tool_calls,
            ..
        } => {
            assert!(content.is_empty(), "{content}");
            assert_eq!(tool_calls[0].name, "echo");
            assert!(!tool_calls[0].id.is_empty());
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        provider.requests()[1].messages.last(),
        Some(Message::Tool { .. })
    ));
}

#[tokio::test]
async fn text_calls_are_left_alone_unless_the_profile_turns_them_on() {
    let dir = tempfile::tempdir().unwrap();
    let text = r#"<tool_call>{"name":"echo","arguments":{"text":"ran"}}</tool_call>"#;
    let provider = MockProvider::new(vec![Script::text(text)]);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert!(finished_outputs(&events).is_empty());
}

#[tokio::test]
async fn a_recovered_call_is_validated_like_any_other() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::text(r#"<tool_call>{"name":"echo","arguments":{"txt":"x"}}</tool_call>"#),
        Script::text("sorry"),
    ]);
    let mut agent = local_agent(provider, dir.path());
    let (_, events) = run(&mut agent, "go").await;
    let (output, is_error) = &finished_outputs(&events)[0];
    assert!(
        *is_error && output.contains("invalid arguments"),
        "{output}"
    );
    assert_eq!(agent.invalid_calls_this_turn(), 1);
}

// Spec: "Write call cut off".
#[tokio::test]
async fn a_call_cut_off_by_the_output_limit_is_not_run() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::Reply(vec![
            Ok(ProviderEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "touch".into(),
                arguments: json!({"path": "cut.txt"}).to_string(),
            })),
            Ok(ProviderEvent::Finished(FinishReason::Length)),
        ]),
        Script::text("I will write it in parts."),
    ]);
    let mut agent = local_agent(provider.clone(), dir.path());
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert!(!dir.path().join("cut.txt").exists());
    let requests = provider.requests();
    match requests[1].messages.last() {
        Some(Message::Tool {
            call_id,
            content,
            is_error,
        }) => {
            assert_eq!(call_id, "c1");
            assert!(*is_error);
            assert!(content.contains("cut off"), "{content}");
            assert!(content.contains("smaller steps"), "{content}");
        }
        other => panic!("{other:?}"),
    }
    assert!(events.iter().any(|e| matches!(e,
        AgentEvent::Warning { message } if message.contains("cut off"))));
}

// Spec: "Answer cut off".
#[tokio::test]
async fn an_answer_cut_off_is_kept_and_continued() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::Reply(vec![
            Ok(ProviderEvent::TextDelta("The first half".into())),
            Ok(ProviderEvent::Finished(FinishReason::Length)),
        ]),
        Script::text(" and the second half."),
    ]);
    let mut agent = local_agent(provider.clone(), dir.path());
    let (reason, _) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    match requests[1].messages.last() {
        Some(Message::User { content }) => {
            assert!(content.starts_with("[harness]"), "{content}");
            assert!(content.contains("cut off"), "{content}");
        }
        other => panic!("{other:?}"),
    }
    let answers: Vec<&str> = agent
        .history()
        .iter()
        .filter_map(|m| match m {
            Message::Assistant { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(answers, ["The first half", " and the second half."]);
}

// A cut-off text call is not recovered: its end is missing.
#[tokio::test]
async fn a_text_call_cut_off_is_not_recovered() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        Script::Reply(vec![
            Ok(ProviderEvent::TextDelta(
                r#"<tool_call>{"name":"echo","arguments":{"text":"x"}}</tool_call>"#.into(),
            )),
            Ok(ProviderEvent::Finished(FinishReason::Length)),
        ]),
        Script::text("ok"),
    ]);
    let mut agent = local_agent(provider, dir.path());
    let (_, events) = run(&mut agent, "go").await;
    assert!(finished_outputs(&events).is_empty());
}
````

In `crates/harness-cli/tests/local_e2e.rs`:

Replace (1 of 2):

```rust
use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;
use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_harness");

fn answer(text: &str) -> ResponseTemplate {
```

with:

```rust
use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;
use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_harness");

fn answer(text: &str) -> ResponseTemplate {
```

Replace (2 of 2):

```rust
            .stderr(contains(flagged).not());
    })
    .await
    .unwrap();
}
```

with:

```rust
            .stderr(contains(flagged).not());
    })
    .await
    .unwrap();
}

// Spec: "Local model emits a tagged tool call as text": a model on a local server gets text tool
// calls by default.
#[tokio::test(flavor = "multi_thread")]
async fn a_local_models_tagged_tool_call_runs() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("\"role\":\"tool\""))
        .and(body_string_contains("pub fn add"))
        .respond_with(answer("It defines add."))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(answer(
            r#"<tool_call>{"name": "read", "arguments": {"path": "src/lib.rs"}}</tool_call>"#,
        ))
        .with_priority(2)
        .mount(&server)
        .await;
    let env = Env::new(&format!(
        "model = \"mock/qwen\"\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\n[profiles.\"mock/*\"]\ncontext_window = 32768\n",
        server.uri()
    ));
    std::fs::create_dir(env.ws.path().join("src")).unwrap();
    std::fs::write(env.ws.path().join("src/lib.rs"), "pub fn add() {}\n").unwrap();
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["ask", "what is in lib.rs?"])
            .assert()
            .success()
            .stdout(contains("It defines add."));
    })
    .await
    .unwrap();
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p harness-core --test local_replies; cargo test -p harness-cli --test local_e2e`
Expected: `local_replies` FAILS to compile: ``unresolved import `harness_core::textcalls` `` and ``no field `text_tool_calls` on type `&mut AgentConfig` ``. `local_e2e` FAILS `a_local_models_tagged_tool_call_runs`: stdout is the `<tool_call>…</tool_call>` text itself, not `It defines add.`; Task 8's two tests pass.

- [ ] **Step 3: Recover calls written as text**

Create `crates/harness-core/src/textcalls.rs`:

```rust
//! Tool calls a model wrote as text instead of as native calls, which local models served
//! without a tool-call parser do. Only a whole message counts: one or more
//! `<tool_call>…</tool_call>` blocks with nothing but whitespace around them, or one JSON object
//! with `name` and `arguments` (or `parameters`, as Llama 3.1 writes it). A call must name a tool
//! the agent has; anything else, a quoted example included, stays text.

use serde_json::Value;

use crate::message::ToolCall;

const OPEN: &str = "<tool_call>";
const CLOSE: &str = "</tool_call>";

/// The calls `text` consists of, when it is nothing but calls to tools `known` names. Their ids
/// are empty; the agent gives them fresh ones.
pub fn recover(text: &str, known: impl Fn(&str) -> bool) -> Option<Vec<ToolCall>> {
    let text = text.trim();
    let calls = if text.starts_with(OPEN) {
        let mut calls = Vec::new();
        let mut rest = text;
        while !rest.is_empty() {
            let inner = rest.strip_prefix(OPEN)?;
            let end = inner.find(CLOSE)?;
            calls.push(call(serde_json::from_str(inner[..end].trim()).ok()?)?);
            rest = inner[end + CLOSE.len()..].trim_start();
        }
        calls
    } else if text.starts_with('{') {
        vec![call(serde_json::from_str(text).ok()?)?]
    } else {
        return None;
    };
    calls.iter().all(|c| known(&c.name)).then_some(calls)
}

/// A call from `{"name": …, "arguments": …}`. Arguments that are an object become JSON text;
/// arguments already given as JSON text are kept as they are.
fn call(value: Value) -> Option<ToolCall> {
    let name = value["name"].as_str().filter(|n| !n.is_empty())?;
    let arguments = match value.get("arguments").or_else(|| value.get("parameters"))? {
        Value::String(text) => text.clone(),
        object @ Value::Object(_) => object.to_string(),
        _ => return None,
    };
    Some(ToolCall {
        id: String::new(),
        name: name.to_string(),
        arguments,
    })
}
```

In `crates/harness-core/src/lib.rs`:

Replace:

```rust
pub mod session;
pub mod subprocess;
pub mod testing;
pub mod time;
pub mod tokens;
pub mod tool;
```

with:

```rust
pub mod session;
pub mod subprocess;
pub mod testing;
pub mod textcalls;
pub mod time;
pub mod tokens;
pub mod tool;
```

- [ ] **Step 4: Handle both in the agent loop**

In `crates/harness-core/src/agent.rs`:

Replace (1 of 6):

```rust
/// Model calls allowed per turn unless configured otherwise.
pub const DEFAULT_MAX_STEPS: u32 = 50;

/// What the rewind list says about effects a rewind cannot undo.
pub const REWIND_LIMITS: &str = "Rewinding restores files in the workspace only: network calls, databases, pushed commits, files outside the workspace, and what is inside nested git repositories and submodules stay as they are. Files that checkpoints leave out (git-ignored files, files over 10 MB, node_modules and target) are neither restored nor removed.";

```

with:

```rust
/// Model calls allowed per turn unless configured otherwise.
pub const DEFAULT_MAX_STEPS: u32 = 50;

/// The result of each tool call of a reply the output limit cut off: the call is not run.
pub const CUT_OFF_CALL: &str = "not run: your reply was cut off at the output-token limit, so this call may be incomplete. Continue in smaller steps: write a large file in parts (write the start, then add the rest with edit), and make one change per call.";

/// The note after a reply without tool calls that the output limit cut off.
pub const CUT_OFF_REPLY: &str = "[harness] Your last reply was cut off at the output-token limit. Continue exactly where it stopped, in smaller steps.";

/// What the rewind list says about effects a rewind cannot undo.
pub const REWIND_LIMITS: &str = "Rewinding restores files in the workspace only: network calls, databases, pushed commits, files outside the workspace, and what is inside nested git repositories and submodules stay as they are. Files that checkpoints leave out (git-ignored files, files over 10 MB, node_modules and target) are neither restored nor removed.";

```

Replace (2 of 6):

```rust
    pub compaction: CompactionConfig,
    /// Output limit, temperature and reasoning effort for every request to the session's model.
    pub request: RequestOptions,
}

impl AgentConfig {
```

with:

```rust
    pub compaction: CompactionConfig,
    /// Output limit, temperature and reasoning effort for every request to the session's model.
    pub request: RequestOptions,
    /// Run tool calls the model writes as text (`textcalls`): for local models.
    pub text_tool_calls: bool,
}

impl AgentConfig {
```

Replace (3 of 6):

```rust
            context_window: DEFAULT_CONTEXT_WINDOW,
            compaction: CompactionConfig::default(),
            request: RequestOptions::default(),
        }
    }
}
```

with:

```rust
            context_window: DEFAULT_CONTEXT_WINDOW,
            compaction: CompactionConfig::default(),
            request: RequestOptions::default(),
            text_tool_calls: false,
        }
    }
}
```

Replace (4 of 6):

```rust
struct ModelReply {
    text: String,
    tool_calls: Vec<ToolCall>,
    #[allow(dead_code)] // read in P4 (truncation)
    finish: Option<FinishReason>,
    /// Whether any output was already shown to the user (then the call must not be retried).
    emitted: bool,
```

with:

```rust
struct ModelReply {
    text: String,
    tool_calls: Vec<ToolCall>,
    finish: Option<FinishReason>,
    /// Whether any output was already shown to the user (then the call must not be retried).
    emitted: bool,
```

Replace (5 of 6):

```rust
                    return self.finish(TurnEndReason::Interrupted, events);
                }
            };
            let calls = reply.tool_calls.clone();
            self.push_assistant(reply.text, calls.clone(), events);
            if calls.is_empty() {
                return self.finish(TurnEndReason::Completed, events);
            }
```

with:

```rust
                    return self.finish(TurnEndReason::Interrupted, events);
                }
            };
            let mut reply = reply;
            let cut_off = reply.finish == Some(FinishReason::Length);
            // A cut-off text call lacks its end, so it is not looked for.
            if reply.tool_calls.is_empty() && !cut_off && self.config.text_tool_calls {
                let tools = &self.tools;
                if let Some(mut calls) =
                    crate::textcalls::recover(&reply.text, |name| tools.get(name).is_some())
                {
                    self.dedupe_call_ids(&mut calls);
                    reply.text.clear();
                    reply.tool_calls = calls;
                }
            }
            let calls = reply.tool_calls.clone();
            self.push_assistant(reply.text, calls.clone(), events);
            if cut_off {
                self.after_cut_off(&calls, events);
                continue;
            }
            if calls.is_empty() {
                return self.finish(TurnEndReason::Completed, events);
            }
```

Replace (6 of 6):

```rust
        self.finish(TurnEndReason::StepLimit, events)
    }

    /// The turn's user message: text parts as they are, and each shell part replaced by the output
    /// of running it as a `bash` tool call, with the same permission check, approval and sandbox.
    /// Once the turn is interrupted, later shell parts are neither run nor asked about.
```

with:

```rust
        self.finish(TurnEndReason::StepLimit, events)
    }

    /// After a reply the output limit cut off: its tool calls get results saying they were not
    /// run, or, without calls, a note asks the model to go on. The turn goes on either way.
    fn after_cut_off(&mut self, calls: &[ToolCall], events: &UnboundedSender<AgentEvent>) {
        let message = if calls.is_empty() {
            self.record(
                Message::User {
                    content: CUT_OFF_REPLY.into(),
                },
                None,
                true,
            );
            "the model's reply was cut off at its output limit; asking it to continue"
        } else {
            for call in calls {
                let result = Message::Tool {
                    call_id: call.id.clone(),
                    content: CUT_OFF_CALL.into(),
                    is_error: true,
                };
                self.record(result, None, false);
            }
            "the model's reply was cut off at its output limit, so its tool calls were not run"
        };
        let _ = events.send(AgentEvent::Warning {
            message: message.into(),
        });
    }

    /// The turn's user message: text parts as they are, and each shell part replaced by the output
    /// of running it as a `bash` tool call, with the same permission check, approval and sandbox.
    /// Once the turn is interrupted, later shell parts are neither run nor asked about.
```

In `crates/harness-cli/src/ask.rs`:

Replace:

```rust
    );
    config.context_window = context_window;
    config.request = profile.request_options();
    if let Some(steps) = setup.config.max_steps {
        config.max_steps = steps;
    }
```

with:

```rust
    );
    config.context_window = context_window;
    config.request = profile.request_options();
    config.text_tool_calls = profile.text_tool_calls;
    if let Some(steps) = setup.config.max_steps {
        config.max_steps = steps;
    }
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p harness-core -p harness-cli`
Expected: PASS: 9 in `local_replies` (among them `text_around_a_call_or_a_quoted_call_is_not_recovered`) and 3 in `local_e2e`; 413 tests in the two crates.

- [ ] **Step 6: Lint**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add crates/harness-core crates/harness-cli
git commit -F - <<'EOF'
feat(core): recover tool calls written as text; handle cut-off replies

With text tool calls on (the default for local models), a reply that
is nothing but <tool_call> blocks, or one JSON object with name and
arguments (or parameters), naming a tool the agent has, runs as those
calls, validated and checked like native ones, and is saved as calls.
Text around them, or a quoted example, stays text. When the output
limit cuts a reply off, its tool calls are not run and each gets a
result saying so, and a reply without calls is kept and followed by a
note asking the model to continue; the turn goes on, and a warning
says what happened.

<trailer lines from the controller>
EOF
```

---

### Task 10: Keep secrets out of what harness writes; `--debug`; the README

**Files:**
- Create: `crates/harness-core/src/redact.rs`, `crates/harness-core/tests/redact.rs`, `crates/harness-cli/tests/redaction_e2e.rs`
- Modify: `crates/harness-core/src/lib.rs`, `crates/harness-core/src/session.rs`, `crates/harness-core/src/output.rs`, `crates/harness-core/src/agent.rs`, `crates/harness-core/tests/tool.rs`, `crates/harness-providers/src/registry.rs`, `crates/harness-providers/src/chatgpt/auth.rs`, `crates/harness-cli/src/setup.rs`, `crates/harness-cli/src/main.rs`, `crates/harness-cli/src/ask.rs`, `crates/harness-cli/tests/cli_smoke.rs`, `crates/harness-cli/tests/auth_e2e.rs`, `README.md`

**Interfaces:**
- Consumes: `Secrets` (Tasks 4 and 6), `ChatGptAuth` (Task 6).
- Produces:
  - `harness_core::redact::{REDACTED = "[redacted]", MIN_SECRET_LEN = 8, Redactor}` with `add(&self, secret)`, `add_env(&self, vars)`, `redact(&self, text) -> String`, shared as `Arc<Redactor>`, with a `Debug` that only counts.
  - `Session::set_redactor(Arc<Redactor>)`; `Agent::with_redactor(Arc<Redactor>) -> Agent` (keeps its redactor across `with_session`); `output::limit_output` gains `redactor: Option<&Redactor>`.
  - `Secrets::redactor(&self) -> Option<Arc<Redactor>>` (default `None`): `resolve` registers every key it hands a provider, and `ChatGptAuth::with_redactor(Arc<Redactor>) -> ChatGptAuth` registers its tokens and every refreshed pair.
  - In the CLI: `Setup::redactor`, holding the secret-named environment values from the start; `--debug` (global; `ask` uses it); `render` writes each event redacted, to NDJSON, the debug log and the terminal alike.

P3's review D noted that session files store tool output verbatim, `printenv` included; this task's canary test runs exactly that.

- [ ] **Step 1: Write the failing tests**

Create `crates/harness-core/tests/redact.rs`:

```rust
//! Secrets stay out of what harness writes (session files, tool-output files), while the model
//! still sees tool output as it is.

mod common;

use std::sync::Arc;

use common::*;
use harness_core::agent::NonInteractive;
use harness_core::message::Message;
use harness_core::permission::Mode;
use harness_core::redact::{REDACTED, Redactor};
use harness_core::session::{EntryKind, Session};
use harness_core::testing::{MockProvider, Script};
use serde_json::json;

const KEY: &str = "sk-canary-0123456789abcdef";

#[test]
fn known_secrets_are_replaced_wherever_they_appear() {
    let redactor = Redactor::default();
    redactor.add(KEY);
    redactor.add("short");
    assert_eq!(
        redactor.redact(&format!("a {KEY} b {KEY}")),
        format!("a {REDACTED} b {REDACTED}")
    );
    // Too short to tell apart from ordinary text.
    assert_eq!(redactor.redact("short text"), "short text");
    // A secret that contains another is replaced whole.
    redactor.add("sk-canary-0123");
    assert_eq!(redactor.redact(KEY), REDACTED);
    assert!(!format!("{redactor:?}").contains("canary"));
}

#[test]
fn a_secret_is_found_in_its_json_escaped_form_too() {
    let redactor = Redactor::default();
    let secret = r#"pa"ss\word-2024"#;
    redactor.add(secret);
    let line = serde_json::to_string(&json!({"content": format!("x {secret} y")})).unwrap();
    let redacted = redactor.redact(&line);
    assert!(!redacted.contains("ss\\\\word"), "{redacted}");
    assert!(redacted.contains(REDACTED), "{redacted}");
}

#[test]
fn secret_looking_environment_variables_are_secrets() {
    let redactor = Redactor::default();
    redactor.add_env(
        [
            ("OPENAI_API_KEY", "sk-proj-aaaaaaaaaaaa"),
            ("GITHUB_TOKEN", "ghp_bbbbbbbbbbbbbbbb"),
            ("AWS_SECRET_ACCESS_KEY", "cccccccccccccccccc"),
            ("DB_PASSWORD", "dddddddddddd"),
            ("client_secret", "eeeeeeeeeeee"),
            ("HOME", "/home/someone"),
            ("SHORT_TOKEN", "abc"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string())),
    );
    let text = "sk-proj-aaaaaaaaaaaa ghp_bbbbbbbbbbbbbbbb cccccccccccccccccc dddddddddddd eeeeeeeeeeee /home/someone abc";
    assert_eq!(
        redactor.redact(text),
        format!("{REDACTED} {REDACTED} {REDACTED} {REDACTED} {REDACTED} /home/someone abc")
    );
}

#[test]
fn session_files_hold_no_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let redactor = Arc::new(Redactor::default());
    redactor.add(KEY);
    let mut session = Session::create(&dir.path().join("sessions"), dir.path());
    session.set_redactor(redactor);
    session.append(EntryKind::Message {
        message: Message::User {
            content: format!("my key is {KEY}"),
        },
        display: None,
        note: false,
    });
    let saved = std::fs::read_to_string(session.path().unwrap()).unwrap();
    assert!(!saved.contains(KEY), "{saved}");
    assert!(saved.contains(&format!("my key is {REDACTED}")), "{saved}");
    // In memory, the conversation is as it was.
    assert!(
        session
            .messages()
            .iter()
            .any(|(_, m)| matches!(m, Message::User { content } if content.contains(KEY)))
    );
}

// Spec: "A command prints the environment", in the core: the tool-output file and the session
// file hold no key, and the model still gets the output as it is.
#[tokio::test]
async fn tool_output_files_and_sessions_hold_no_secrets_but_the_model_sees_the_output() {
    let dir = tempfile::tempdir().unwrap();
    let long = format!(
        "{}\nOPENAI_API_KEY={KEY}\n{}",
        "a".repeat(8_000),
        "b".repeat(8_000)
    );
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "echo", json!({"text": long})),
        Script::text("done"),
    ]);
    let redactor = Arc::new(Redactor::default());
    redactor.add(KEY);
    let sessions = dir.path().join("sessions");
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    )
    .with_session(Session::create(&sessions, dir.path()))
    .with_redactor(redactor);
    agent.config_mut().output_limit = 4_000;
    run(&mut agent, "go").await;
    let spilled = std::fs::read_to_string(dir.path().join(".spill/c1.txt")).unwrap();
    assert!(!spilled.contains(KEY));
    assert!(spilled.contains(&format!("OPENAI_API_KEY={REDACTED}")));
    for file in std::fs::read_dir(&sessions).unwrap() {
        let saved = std::fs::read_to_string(file.unwrap().path()).unwrap();
        assert!(!saved.contains(KEY), "{saved}");
    }
    // The call's arguments carried the key to the model's own request, as it wrote them.
    let requests = provider.requests();
    assert!(
        serde_json::to_string(&requests[1].messages)
            .unwrap()
            .contains(KEY)
    );
}
```

Create `crates/harness-cli/tests/redaction_e2e.rs`:

```rust
//! The canary test for task 4.7: a run whose API key and a secret-looking environment variable
//! reach the conversation through `printenv` leaves neither in the session file, the tool-output
//! files, the debug log, the NDJSON output or what harness prints.

use assert_cmd::Command;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_harness");
const KEY: &str = "sk-canary-key-0123456789";
const TOKEN: &str = "canary-token-9876543210";

fn stream(chunk: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(
        format!("data: {chunk}\n\ndata: [DONE]\n\n"),
        "text/event-stream",
    )
}

/// Every file under `dir`, as text.
fn files(dir: &std::path::Path) -> Vec<(std::path::PathBuf, String)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(files(&path));
        } else if let Ok(text) = std::fs::read_to_string(&path) {
            found.push((path, text));
        }
    }
    found
}

// Spec: "Debug logging" and "A command prints the environment".
#[tokio::test(flavor = "multi_thread")]
async fn no_secret_is_written_anywhere() {
    let server = MockServer::start().await;
    // Second request: the tool ran. The model repeats the key it saw.
    Mock::given(method("POST"))
        .and(body_string_contains("\"role\":\"tool\""))
        .respond_with(stream(json!({"choices": [{"index": 0,
            "delta": {"content": format!("Your key is {KEY}.")}, "finish_reason": "stop"}]})))
        .with_priority(1)
        .mount(&server)
        .await;
    // The secrets first, so that the model's share of the output holds them; then enough output
    // that it is saved to a tool-output file.
    let command = "printenv DEPLOY_TOKEN MOCK_API_KEY; printenv; seq 1 4000";
    Mock::given(method("POST"))
        .respond_with(stream(json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0,
            "id": "c1", "type": "function", "function": {"name": "bash",
            "arguments": json!({"command": command}).to_string()}}]}, "finish_reason": "tool_calls"}]})))
        .with_priority(2)
        .mount(&server)
        .await;
    let home = TempDir::new().unwrap();
    let ws = TempDir::new().unwrap();
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::write(
        home.path().join("config/config.toml"),
        format!(
            "model = \"mock/m\"\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\napi_key_env = \"MOCK_API_KEY\"\n[profiles.\"mock/*\"]\ncontext_window = 32768\n",
            server.uri()
        ),
    )
    .unwrap();
    std::fs::create_dir(ws.path().join(".git")).unwrap();
    let (home, ws, output) = tokio::task::spawn_blocking(move || {
        let output = Command::new(BIN)
            .current_dir(ws.path())
            .env("HARNESS_HOME", home.path())
            .env("HARNESS_CREDENTIAL_STORE", "file")
            .env("MOCK_API_KEY", KEY)
            .env("DEPLOY_TOKEN", TOKEN)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME")
            .args(["--debug", "ask", "--json", "show the env"])
            .output()
            .unwrap();
        (home, ws, output)
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    for (what, text) in [("stdout", stdout.as_ref()), ("stderr", stderr.as_ref())] {
        assert!(
            !text.contains(KEY) && !text.contains(TOKEN),
            "{what}: {text}"
        );
    }
    assert!(stdout.contains("[redacted]"), "{stdout}");
    assert!(stderr.contains("debug log: "), "{stderr}");
    let written = files(home.path());
    let names: Vec<String> = written
        .iter()
        .map(|(p, _)| p.display().to_string())
        .collect();
    for kind in ["/data/sessions/", "/state/tool-output/", "/state/logs/"] {
        assert!(
            names.iter().any(|n| n.contains(kind)),
            "no {kind} file: {names:?}"
        );
    }
    for (path, text) in &written {
        assert!(
            !text.contains(KEY) && !text.contains(TOKEN),
            "{} holds a secret",
            path.display()
        );
    }
    // The model still saw the output as it was.
    let requests = server.received_requests().await.unwrap();
    assert!(String::from_utf8_lossy(&requests[1].body).contains(TOKEN));
    drop(ws);
}
```

Append to `crates/harness-cli/tests/cli_smoke.rs`, after a blank line:

```rust
#[test]
fn help_lists_the_debug_flag() {
    Command::new(env!("CARGO_BIN_EXE_harness"))
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("--debug").and(contains("secrets redacted")));
}
```

Task 4's end-to-end test answered with the key a request carried; redacted, it would no longer read back. It names the key instead. In `crates/harness-cli/tests/auth_e2e.rs`:

Replace (1 of 4):

```rust
    )
}

/// Answers with the key a request carried.
async fn echo_keys(server: &MockServer) {
    for key in ["sk-stored", "sk-env", "sk-work"] {
        Mock::given(method("POST"))
            .and(header("authorization", format!("Bearer {key}").as_str()))
            .respond_with(answer(&format!("used {key}")))
            .mount(server)
            .await;
    }
```

with:

```rust
    )
}

/// Answers with which key a request carried, by name: an answer that repeated the key would be
/// redacted.
async fn echo_keys(server: &MockServer) {
    for (key, name) in [
        ("sk-stored", "the stored key"),
        ("sk-env", "the environment's key"),
        ("sk-work", "the work key"),
    ] {
        Mock::given(method("POST"))
            .and(header("authorization", format!("Bearer {key}").as_str()))
            .respond_with(answer(&format!("used {name}")))
            .mount(server)
            .await;
    }
```

Replace (2 of 4):

```rust
            .args(["--model", "mock/m", "ask", "hi"])
            .assert()
            .success()
            .stdout(contains("used sk-stored"));
        env.cmd()
            .env("MOCK_API_KEY", "sk-env")
            .args(["--model", "mock/m", "ask", "hi"])
            .assert()
            .success()
            .stdout(contains("used sk-env"));
    })
    .await
    .unwrap();
```

with:

```rust
            .args(["--model", "mock/m", "ask", "hi"])
            .assert()
            .success()
            .stdout(contains("used the stored key"));
        env.cmd()
            .env("MOCK_API_KEY", "sk-env")
            .args(["--model", "mock/m", "ask", "hi"])
            .assert()
            .success()
            .stdout(contains("used the environment's key"));
    })
    .await
    .unwrap();
```

Replace (3 of 4):

```rust
            .args(["--model", "mock/m", "ask", "hi"])
            .assert()
            .success()
            .stdout(contains("used sk-work"));
        env.cmd()
            .args(["logout", "mock"])
            .assert()
```

with:

```rust
            .args(["--model", "mock/m", "ask", "hi"])
            .assert()
            .success()
            .stdout(contains("used the work key"));
        env.cmd()
            .args(["logout", "mock"])
            .assert()
```

Replace (4 of 4):

```rust
            .args(["--model", "mock/m", "ask", "hi"])
            .assert()
            .success()
            .stdout(contains("used sk-stored"));
    })
    .await
    .unwrap();
```

with:

```rust
            .args(["--model", "mock/m", "ask", "hi"])
            .assert()
            .success()
            .stdout(contains("used the stored key"));
    })
    .await
    .unwrap();
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p harness-core --test redact; cargo test -p harness-cli --test redaction_e2e; cargo test -p harness-cli --test cli_smoke; cargo test -p harness-cli --test auth_e2e`
Expected: `redact` FAILS to compile: ``unresolved import `harness_core::redact` ``, ``no method named `set_redactor` found for struct `harness_core::session::Session` ``, ``no method named `with_redactor` found for struct `Agent` ``. `redaction_e2e` FAILS with `error: unexpected argument '--debug' found`. `cli_smoke` FAILS `help_lists_the_debug_flag`; its other 8 pass. `auth_e2e` passes (4): the change only stops it relying on the key being printed.

- [ ] **Step 3: The redactor, and the files the core writes**

Create `crates/harness-core/src/redact.rs`:

```rust
//! Secrets harness knows, kept out of everything it writes: session files, tool-output files, the
//! debug log, NDJSON and what it prints. They are the API keys and tokens it uses and the values
//! of environment variables whose names mark them as secrets. What the model is sent is left as
//! it is, so a file it reads and writes back keeps its real contents.

use std::sync::RwLock;

/// What a secret is replaced with.
pub const REDACTED: &str = "[redacted]";
/// Shorter values are not treated as secrets: they would match ordinary text.
pub const MIN_SECRET_LEN: usize = 8;

/// The secrets to keep out of what harness writes. Shared, and added to as tokens are refreshed.
#[derive(Default)]
pub struct Redactor {
    secrets: RwLock<Vec<String>>,
}

/// Shows how many secrets it holds, never the secrets.
impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.secrets.read().map(|s| s.len()).unwrap_or(0);
        write!(f, "Redactor({count} secrets)")
    }
}

impl Redactor {
    /// Adds `secret`, and the form it takes inside a JSON string, unless it is shorter than
    /// [`MIN_SECRET_LEN`].
    pub fn add(&self, secret: &str) {
        let secret = secret.trim();
        if secret.len() < MIN_SECRET_LEN {
            return;
        }
        let quoted = serde_json::to_string(secret).expect("a string serializes");
        let escaped = &quoted[1..quoted.len() - 1];
        let mut secrets = self.secrets.write().expect("secrets lock");
        for form in [secret, escaped] {
            if !secrets.iter().any(|s| s == form) {
                secrets.push(form.to_string());
            }
        }
        // Longest first, so that a secret containing another is replaced whole.
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    }

    /// Adds the values of the variables in `vars` whose names end in `KEY`, `TOKEN`, `SECRET` or
    /// `PASSWORD`, in any case.
    pub fn add_env(&self, vars: impl IntoIterator<Item = (String, String)>) {
        for (name, value) in vars {
            let name = name.to_ascii_uppercase();
            if ["KEY", "TOKEN", "SECRET", "PASSWORD"]
                .iter()
                .any(|suffix| name.ends_with(suffix))
            {
                self.add(&value);
            }
        }
    }

    /// `text` with every secret replaced by [`REDACTED`].
    pub fn redact(&self, text: &str) -> String {
        let secrets = self.secrets.read().expect("secrets lock");
        let mut text = text.to_string();
        for secret in secrets.iter() {
            if text.contains(secret.as_str()) {
                text = text.replace(secret.as_str(), REDACTED);
            }
        }
        text
    }
}
```

In `crates/harness-core/src/lib.rs`:

Replace:

```rust
pub mod output;
pub mod permission;
pub mod provider;
pub mod retry;
pub mod session;
pub mod subprocess;
```

with:

```rust
pub mod output;
pub mod permission;
pub mod provider;
pub mod redact;
pub mod retry;
pub mod session;
pub mod subprocess;
```

In `crates/harness-core/src/session.rs`:

Replace (1 of 7):

```rust
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde::{Deserialize, Serialize};

use crate::{compaction, message::Message, time};

/// The session file format written by this version. Version 2 added the workspace of each
/// checkpoint: a harness that reads version 1 only would restore them in the wrong place. A file
```

with:

```rust
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use serde::{Deserialize, Serialize};

use crate::{compaction, message::Message, redact::Redactor, time};

/// The session file format written by this version. Version 2 added the workspace of each
/// checkpoint: a harness that reads version 1 only would restore them in the wrong place. A file
```

Replace (2 of 7):

```rust
    /// The file is in an older format than this harness writes, so a [`EntryKind::Version`]
    /// entry goes before the next entry appended.
    record_version: bool,
}

/// Where a session is saved. The file is created, and locked, when the first entry after the
```

with:

```rust
    /// The file is in an older format than this harness writes, so a [`EntryKind::Version`]
    /// entry goes before the next entry appended.
    record_version: bool,
    /// Keeps secrets out of the file; the entries in memory stay as they are.
    redactor: Option<Arc<Redactor>>,
}

/// Where a session is saved. The file is created, and locked, when the first entry after the
```

Replace (3 of 7):

```rust
            save_error: None,
            warnings: Vec::new(),
            record_version: false,
        }
    }

```

with:

```rust
            save_error: None,
            warnings: Vec::new(),
            record_version: false,
            redactor: None,
        }
    }

```

Replace (4 of 7):

```rust
            save_error: None,
            warnings: Vec::new(),
            record_version: newest < FORMAT_VERSION,
        };
        if let Some(problem) = session.walk().1 {
            warnings.push(format!("{}: {problem}", path.display()));
```

with:

```rust
            save_error: None,
            warnings: Vec::new(),
            record_version: newest < FORMAT_VERSION,
            redactor: None,
        };
        if let Some(problem) = session.walk().1 {
            warnings.push(format!("{}: {problem}", path.display()));
```

Replace (5 of 7):

```rust
        let Some(store) = self.store.as_mut() else {
            return;
        };
        if let Err(e) = store.write(&self.entries, &mut self.warnings) {
            self.store = None;
            self.save_error = Some(e);
        }
    }

    /// Why the session stopped saving, the first time it is asked after that happened.
    pub fn take_save_error(&mut self) -> Option<std::io::Error> {
        self.save_error.take()
```

with:

```rust
        let Some(store) = self.store.as_mut() else {
            return;
        };
        if let Err(e) = store.write(&self.entries, &mut self.warnings, self.redactor.as_deref()) {
            self.store = None;
            self.save_error = Some(e);
        }
    }

    /// Keeps the secrets `redactor` knows out of the file from now on.
    pub fn set_redactor(&mut self, redactor: Arc<Redactor>) {
        self.redactor = Some(redactor);
    }

    /// Why the session stopped saving, the first time it is asked after that happened.
    pub fn take_save_error(&mut self) -> Option<std::io::Error> {
        self.save_error.take()
```

Replace (6 of 7):

```rust
}

impl Store {
    /// Writes the entries not yet in the file; a warning about it goes to `warnings`.
    fn write(&mut self, entries: &[Entry], warnings: &mut Vec<String>) -> std::io::Result<()> {
        if self.file.is_none() {
            // Sessions hold prompts, code and tool output: only their owner may read them.
            if let Some(dir) = self.path.parent() {
```

with:

```rust
}

impl Store {
    /// Writes the entries not yet in the file, without the secrets `redactor` knows; a warning
    /// about it goes to `warnings`.
    fn write(
        &mut self,
        entries: &[Entry],
        warnings: &mut Vec<String>,
        redactor: Option<&Redactor>,
    ) -> std::io::Result<()> {
        if self.file.is_none() {
            // Sessions hold prompts, code and tool output: only their owner may read them.
            if let Some(dir) = self.path.parent() {
```

Replace (7 of 7):

```rust
        let file = self.file.as_mut().expect("opened above");
        let mut text = String::new();
        for entry in &entries[self.written..] {
            text.push_str(&serde_json::to_string(entry).map_err(std::io::Error::other)?);
            text.push('\n');
        }
        // One write per batch, so a crash leaves at most one incomplete line.
```

with:

```rust
        let file = self.file.as_mut().expect("opened above");
        let mut text = String::new();
        for entry in &entries[self.written..] {
            let line = serde_json::to_string(entry).map_err(std::io::Error::other)?;
            match redactor {
                Some(redactor) => text.push_str(&redactor.redact(&line)),
                None => text.push_str(&line),
            }
            text.push('\n');
        }
        // One write per batch, so a crash leaves at most one incomplete line.
```

In `crates/harness-core/src/output.rs`:

Replace (1 of 2):

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
```

with:

```rust
use std::path::Path;

use crate::redact::Redactor;

/// Tool output above this many bytes is saved to a file instead of being sent whole.
pub const DEFAULT_OUTPUT_LIMIT: usize = 10 * 1024;

/// Caps tool output at roughly `limit` bytes. Larger output is saved in full to `dir/<call_id>.txt`,
/// without the secrets `redactor` knows; the model receives the head, the tail, the omitted size,
/// and the file path.
pub fn limit_output(
    content: &str,
    limit: usize,
    dir: &Path,
    call_id: &str,
    redactor: Option<&Redactor>,
) -> String {
    if content.len() <= limit {
        return content.to_string();
    }
```

Replace (2 of 2):

```rust
        })
        .collect();
    let file = dir.join(format!("{safe_id}.txt"));
    let saved = std::fs::create_dir_all(dir).and_then(|_| std::fs::write(&file, content));

    let keep = limit * 2 / 5;
    let head_end = floor_boundary(content, keep);
```

with:

```rust
        })
        .collect();
    let file = dir.join(format!("{safe_id}.txt"));
    let saved = std::fs::create_dir_all(dir).and_then(|_| {
        let content = redactor.map_or_else(|| content.to_string(), |r| r.redact(content));
        std::fs::write(&file, content)
    });

    let keep = limit * 2 / 5;
    let head_end = floor_boundary(content, keep);
```

In `crates/harness-core/src/agent.rs`:

Replace (1 of 5):

```rust
    output::{DEFAULT_OUTPUT_LIMIT, limit_output},
    permission::{Action, Decision, FsAccess, Mode, PermissionPolicy},
    provider::{FinishReason, Provider, ProviderError, ProviderEvent},
    retry::RetryPolicy,
    session::{Entry, EntryKind, RewindScope, Session},
    tokens::DEFAULT_CONTEXT_WINDOW,
```

with:

```rust
    output::{DEFAULT_OUTPUT_LIMIT, limit_output},
    permission::{Action, Decision, FsAccess, Mode, PermissionPolicy},
    provider::{FinishReason, Provider, ProviderError, ProviderEvent},
    redact::Redactor,
    retry::RetryPolicy,
    session::{Entry, EntryKind, RewindScope, Session},
    tokens::DEFAULT_CONTEXT_WINDOW,
```

Replace (2 of 5):

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
    /// Keeps secrets out of the session file and tool-output files.
    redactor: Option<Arc<Redactor>>,
}

impl Agent {
```

Replace (3 of 5):

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
            redactor: None,
        }
    }

```

Replace (4 of 5):

```rust
    /// a stopped run left without results at the end of it get results, saved in the session.
    pub fn with_session(mut self, session: Session) -> Self {
        self.session = session;
        self.load_history(true);
        self
    }

    /// Rebuilds the history from the session's active branch. Every tool call needs a result, or
    /// providers reject the request, but a run that was killed while a tool ran left none: such
    /// a call gets one saying its effects are unknown. At the end of the branch that result is
```

with:

```rust
    /// a stopped run left without results at the end of it get results, saved in the session.
    pub fn with_session(mut self, session: Session) -> Self {
        self.session = session;
        if let Some(redactor) = &self.redactor {
            self.session.set_redactor(redactor.clone());
        }
        self.load_history(true);
        self
    }

    /// Keeps the secrets `redactor` knows out of the session file and tool-output files. The
    /// model is still sent everything as it is.
    pub fn with_redactor(mut self, redactor: Arc<Redactor>) -> Self {
        self.session.set_redactor(redactor.clone());
        self.redactor = Some(redactor);
        self
    }

    /// Rebuilds the history from the session's active branch. Every tool call needs a result, or
    /// providers reject the request, but a run that was killed while a tool ran left none: such
    /// a call gets one saying its effects are unknown. At the end of the branch that result is
```

Replace (5 of 5):

```rust
            self.config.output_limit,
            &self.config.output_dir,
            &call.id,
        );
        let output = ToolOutput { content, ..raw };
        let _ = events.send(AgentEvent::ToolCallFinished {
```

with:

```rust
            self.config.output_limit,
            &self.config.output_dir,
            &call.id,
            self.redactor.as_deref(),
        );
        let output = ToolOutput { content, ..raw };
        let _ = events.send(AgentEvent::ToolCallFinished {
```

`limit_output`'s own tests pass no redactor. In `crates/harness-core/tests/tool.rs`:

Replace (1 of 3):

```rust
fn small_output_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        limit_output("hello", DEFAULT_OUTPUT_LIMIT, dir.path(), "c1"),
        "hello"
    );
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
```

with:

```rust
fn small_output_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        limit_output("hello", DEFAULT_OUTPUT_LIMIT, dir.path(), "c1", None),
        "hello"
    );
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
```

Replace (2 of 3):

```rust
fn large_output_is_spilled_to_a_file_with_head_and_tail() {
    let dir = tempfile::tempdir().unwrap();
    let content: String = (1..=5000).map(|i| format!("line {i}\n")).collect();
    let limited = limit_output(&content, 1000, dir.path(), "call/7");
    assert!(limited.starts_with("line 1\n"));
    assert!(limited.trim_end().ends_with("line 5000"));
    assert!(limited.contains("omitted"));
```

with:

```rust
fn large_output_is_spilled_to_a_file_with_head_and_tail() {
    let dir = tempfile::tempdir().unwrap();
    let content: String = (1..=5000).map(|i| format!("line {i}\n")).collect();
    let limited = limit_output(&content, 1000, dir.path(), "call/7", None);
    assert!(limited.starts_with("line 1\n"));
    assert!(limited.trim_end().ends_with("line 5000"));
    assert!(limited.contains("omitted"));
```

Replace (3 of 3):

```rust
fn limiting_never_splits_a_multibyte_character() {
    let dir = tempfile::tempdir().unwrap();
    let content = "é".repeat(5000);
    let limited = limit_output(&content, 999, dir.path(), "u");
    assert!(limited.contains("omitted"));
}

```

with:

```rust
fn limiting_never_splits_a_multibyte_character() {
    let dir = tempfile::tempdir().unwrap();
    let content = "é".repeat(5000);
    let limited = limit_output(&content, 999, dir.path(), "u", None);
    assert!(limited.contains("omitted"));
}

```

- [ ] **Step 4: Register the keys and tokens providers are given**

In `crates/harness-providers/src/registry.rs`:

Replace (1 of 4):

```rust
use std::{collections::BTreeMap, sync::Arc};

use crate::credentials::Credentials;

use harness_config::config::{Protocol, ProviderConfig};
```

with:

```rust
use std::{collections::BTreeMap, sync::Arc};

use harness_core::redact::Redactor;

use crate::credentials::Credentials;

use harness_config::config::{Protocol, ProviderConfig};
```

Replace (2 of 4):

```rust
    fn credentials(&self) -> Option<Arc<Credentials>> {
        None
    }
}

impl<F: Fn(&str) -> Option<String>> Secrets for F {
```

with:

```rust
    fn credentials(&self) -> Option<Arc<Credentials>> {
        None
    }

    /// Where the keys and tokens a provider is given are registered as secrets, so that nothing
    /// harness writes holds them.
    fn redactor(&self) -> Option<Arc<Redactor>> {
        None
    }
}

impl<F: Fn(&str) -> Option<String>> Secrets for F {
```

Replace (3 of 4):

```rust
            var,
        });
    }
    // Claude Free/Pro/Max credentials may only be used by Claude Code itself.
    if protocol == Protocol::AnthropicMessages
        && api_key.as_deref().is_some_and(is_claude_subscription_token)
```

with:

```rust
            var,
        });
    }
    if let (Some(redactor), Some(key)) = (secrets.redactor(), &api_key) {
        redactor.add(key);
    }
    // Claude Free/Pro/Max credentials may only be used by Claude Code itself.
    if protocol == Protocol::AnthropicMessages
        && api_key.as_deref().is_some_and(is_claude_subscription_token)
```

Replace (4 of 4):

```rust
    let issuer = secrets
        .env("HARNESS_CHATGPT_ISSUER")
        .unwrap_or_else(|| ISSUER.to_string());
    let auth = ChatGptAuth::load(credentials, &profile, OAuth::new(&issuer))
        .ok()
        .flatten()
        .ok_or_else(not_signed_in)?;
    let base_url = secrets
        .env("HARNESS_CHATGPT_BASE_URL")
        .unwrap_or_else(|| BASE_URL.to_string());
```

with:

```rust
    let issuer = secrets
        .env("HARNESS_CHATGPT_ISSUER")
        .unwrap_or_else(|| ISSUER.to_string());
    let mut auth = ChatGptAuth::load(credentials, &profile, OAuth::new(&issuer))
        .ok()
        .flatten()
        .ok_or_else(not_signed_in)?;
    if let Some(redactor) = secrets.redactor() {
        auth = auth.with_redactor(redactor);
    }
    let base_url = secrets
        .env("HARNESS_CHATGPT_BASE_URL")
        .unwrap_or_else(|| BASE_URL.to_string());
```

In `crates/harness-providers/src/chatgpt/auth.rs`:

Replace (1 of 4):

```rust

use std::{sync::Arc, time::Duration};

use harness_core::{provider::ProviderError, time::now_unix};
use tokio::sync::Mutex;

use super::oauth::{OAuth, OAuthError, Tokens};
```

with:

```rust

use std::{sync::Arc, time::Duration};

use harness_core::{provider::ProviderError, redact::Redactor, time::now_unix};
use tokio::sync::Mutex;

use super::oauth::{OAuth, OAuthError, Tokens};
```

Replace (2 of 4):

```rust
    credentials: Arc<Credentials>,
    profile: String,
    tokens: Mutex<Tokens>,
}

impl ChatGptAuth {
```

with:

```rust
    credentials: Arc<Credentials>,
    profile: String,
    tokens: Mutex<Tokens>,
    /// Where the tokens, and those that replace them, are registered as secrets.
    redactor: Option<Arc<Redactor>>,
}

impl ChatGptAuth {
```

Replace (3 of 4):

```rust
            credentials,
            profile: profile.to_string(),
            tokens: Mutex::new(tokens),
        }))
    }

    /// The tokens for the next request, refreshed first when the access token is about to
    /// expire.
    pub async fn current(&self) -> Result<Tokens, ProviderError> {
```

with:

```rust
            credentials,
            profile: profile.to_string(),
            tokens: Mutex::new(tokens),
            redactor: None,
        }))
    }

    /// Registers the tokens, now and after each refresh, as secrets with `redactor`.
    pub fn with_redactor(mut self, redactor: Arc<Redactor>) -> ChatGptAuth {
        let tokens = self.tokens.get_mut();
        redactor.add(&tokens.access_token);
        redactor.add(&tokens.refresh_token);
        self.redactor = Some(redactor);
        self
    }

    /// The tokens for the next request, refreshed first when the access token is about to
    /// expire.
    pub async fn current(&self) -> Result<Tokens, ProviderError> {
```

Replace (4 of 4):

```rust
        Ok(tokens.clone())
    }

    /// Replaces `tokens`, whose access token `used` is no good: with the stored tokens when
    /// another process has renewed them, else with refreshed ones, which are then stored.
    async fn renew(&self, tokens: &mut Tokens, used: &str) -> Result<(), ProviderError> {
        if let Ok(Some(theirs)) = stored(&self.credentials, &self.profile)
            && theirs.access_token != used
        {
            *tokens = theirs;
            if !expiring(tokens) {
                return Ok(());
            }
        }
        let fresh = self.oauth.refresh(tokens).await.map_err(refresh_error)?;
        self.credentials
            .set(PROVIDER, &self.profile, &fresh.to_json())
            .map_err(|e| {
```

with:

```rust
        Ok(tokens.clone())
    }

    /// Registers `tokens` as secrets.
    fn register(&self, tokens: &Tokens) {
        if let Some(redactor) = &self.redactor {
            redactor.add(&tokens.access_token);
            redactor.add(&tokens.refresh_token);
        }
    }

    /// Replaces `tokens`, whose access token `used` is no good: with the stored tokens when
    /// another process has renewed them, else with refreshed ones, which are then stored.
    async fn renew(&self, tokens: &mut Tokens, used: &str) -> Result<(), ProviderError> {
        if let Ok(Some(theirs)) = stored(&self.credentials, &self.profile)
            && theirs.access_token != used
        {
            self.register(&theirs);
            *tokens = theirs;
            if !expiring(tokens) {
                return Ok(());
            }
        }
        let fresh = self.oauth.refresh(tokens).await.map_err(refresh_error)?;
        self.register(&fresh);
        self.credentials
            .set(PROVIDER, &self.profile, &fresh.to_json())
            .map_err(|e| {
```

- [ ] **Step 5: Redact what the CLI prints; add `--debug`**

Everything `render` shows comes from the redacted event, so NDJSON, the debug log, stderr and the final answer on stdout hold the same text.

In `crates/harness-cli/src/setup.rs`:

Replace (1 of 6):

```rust
    paths::Paths,
    trust::TrustStore,
};
use harness_providers::{credentials::Credentials, registry::Secrets};

/// Everything a command needs about where it runs.
```

with:

```rust
    paths::Paths,
    trust::TrustStore,
};
use harness_core::redact::Redactor;
use harness_providers::{credentials::Credentials, registry::Secrets};

/// Everything a command needs about where it runs.
```

Replace (2 of 6):

```rust
    pub trust: TrustStore,
    /// Stored API keys and sign-in tokens.
    pub credentials: Arc<Credentials>,
}

impl Setup {
```

with:

```rust
    pub trust: TrustStore,
    /// Stored API keys and sign-in tokens.
    pub credentials: Arc<Credentials>,
    /// The secrets nothing harness writes may hold: those in the environment from the start, and
    /// each key or token a provider is given.
    pub redactor: Arc<Redactor>,
}

impl Setup {
```

Replace (3 of 6):

```rust
    pub fn keys(&self) -> Keys<'_> {
        Keys {
            credentials: &self.credentials,
        }
    }
}
```

with:

```rust
    pub fn keys(&self) -> Keys<'_> {
        Keys {
            credentials: &self.credentials,
            redactor: &self.redactor,
        }
    }
}
```

Replace (4 of 6):

```rust
#[derive(Clone, Copy)]
pub struct Keys<'a> {
    credentials: &'a Arc<Credentials>,
}

impl Secrets for Keys<'_> {
```

with:

```rust
#[derive(Clone, Copy)]
pub struct Keys<'a> {
    credentials: &'a Arc<Credentials>,
    redactor: &'a Arc<Redactor>,
}

impl Secrets for Keys<'_> {
```

Replace (5 of 6):

```rust
    fn credentials(&self) -> Option<Arc<Credentials>> {
        Some(self.credentials.clone())
    }
}

/// Loads paths and configuration. Errors are user-facing messages (exit code 2).
```

with:

```rust
    fn credentials(&self) -> Option<Arc<Credentials>> {
        Some(self.credentials.clone())
    }

    fn redactor(&self) -> Option<Arc<Redactor>> {
        Some(self.redactor.clone())
    }
}

/// Loads paths and configuration. Errors are user-facing messages (exit code 2).
```

Replace (6 of 6):

```rust
        eprintln!("warning: {}", crate::term::terminal_safe(warning));
    }
    let credentials = Arc::new(Credentials::open(&paths.data_dir, env));
    Ok(Setup {
        paths,
        config,
        workspace,
        trust,
        credentials,
    })
}

```

with:

```rust
        eprintln!("warning: {}", crate::term::terminal_safe(warning));
    }
    let credentials = Arc::new(Credentials::open(&paths.data_dir, env));
    let redactor = Arc::new(Redactor::default());
    redactor.add_env(std::env::vars());
    Ok(Setup {
        paths,
        config,
        workspace,
        trust,
        credentials,
        redactor,
    })
}

```

In `crates/harness-cli/src/main.rs`:

Replace (1 of 2):

```rust
    /// Continue the most recent session in this project
    #[arg(short = 'c', long = "continue", global = true)]
    continue_session: bool,
    /// Resume the session with this id; without an id, list this project's sessions
    #[arg(
        long,
```

with:

```rust
    /// Continue the most recent session in this project
    #[arg(short = 'c', long = "continue", global = true)]
    continue_session: bool,
    /// Also write the run's events, secrets redacted, to a log file in the state directory
    #[arg(long, global = true)]
    debug: bool,
    /// Resume the session with this id; without an id, list this project's sessions
    #[arg(
        long,
```

Replace (2 of 2):

```rust
    let code = runtime.block_on(async move {
        match cli.command {
            Some(Command::Ask { json, prompt }) => {
                ask::run(cli.model, cli.mode, session, prompt.join(" "), json).await
            }
            Some(Command::Models) => models::run().await,
            Some(Command::Auth {
```

with:

```rust
    let code = runtime.block_on(async move {
        match cli.command {
            Some(Command::Ask { json, prompt }) => {
                ask::run(
                    cli.model,
                    cli.mode,
                    session,
                    prompt.join(" "),
                    json,
                    cli.debug,
                )
                .await
            }
            Some(Command::Models) => models::run().await,
            Some(Command::Auth {
```

In `crates/harness-cli/src/ask.rs`:

Replace (1 of 7):

```rust
    engine::{EngineConfig, PermissionEngine, RuleSet},
    event::{AgentEvent, TurnEndReason},
    permission::{FsAccess, Mode},
    tool::{CommandSandbox, ToolContext},
};
use harness_providers::{
```

with:

```rust
    engine::{EngineConfig, PermissionEngine, RuleSet},
    event::{AgentEvent, TurnEndReason},
    permission::{FsAccess, Mode},
    redact::Redactor,
    tool::{CommandSandbox, ToolContext},
};
use harness_providers::{
```

Replace (2 of 7):

```rust
    session: crate::sessions::Choice,
    prompt_text: String,
    json: bool,
) -> u8 {
    // Registered as the very first thing this function does (a plain synchronous call, not an
    // awaited future): once it returns, the OS delivers SIGINT to tokio's signal driver instead of
```

with:

```rust
    session: crate::sessions::Choice,
    prompt_text: String,
    json: bool,
    debug: bool,
) -> u8 {
    // Registered as the very first thing this function does (a plain synchronous call, not an
    // awaited future): once it returns, the OS delivers SIGINT to tokio's signal driver instead of
```

Replace (3 of 7):

```rust
            .unwrap_or(0),
        std::process::id()
    );
    let output_dir = setup.paths.state_dir.join("tool-output").join(run_id);
    let sandbox_disabled_by_env =
        std::env::var("HARNESS_SANDBOX").as_deref() == Ok("none") && mode != Mode::FullAccess;
```

with:

```rust
            .unwrap_or(0),
        std::process::id()
    );
    let log = if debug {
        match open_log(&setup.paths.state_dir, &run_id) {
            Ok((file, path)) => {
                eprintln!("debug log: {}", terminal_safe(&path.display().to_string()));
                Some(file)
            }
            Err(e) => {
                eprintln!("warning: cannot write the debug log: {e}");
                None
            }
        }
    } else {
        None
    };
    let output_dir = setup.paths.state_dir.join("tool-output").join(run_id);
    let sandbox_disabled_by_env =
        std::env::var("HARNESS_SANDBOX").as_deref() == Ok("none") && mode != Mode::FullAccess;
```

Replace (4 of 7):

```rust
        config,
        ctx,
    )
    .with_session(session)
    .with_checkpoints(checkpoints);

    let (tx, rx) = mpsc::unbounded_channel();
    let renderer = tokio::spawn(render(rx, json, cancel.clone()));
    let reason = agent.run_turn(turn, &tx, cancel).await;
    drop(tx);
    let (final_text, blocked) = renderer.await.unwrap_or_default();
```

with:

```rust
        config,
        ctx,
    )
    .with_redactor(setup.redactor.clone())
    .with_session(session)
    .with_checkpoints(checkpoints);

    let (tx, rx) = mpsc::unbounded_channel();
    let renderer = tokio::spawn(render(
        rx,
        json,
        cancel.clone(),
        setup.redactor.clone(),
        log,
    ));
    let reason = agent.run_turn(turn, &tx, cancel).await;
    drop(tx);
    let (final_text, blocked) = renderer.await.unwrap_or_default();
```

Replace (5 of 7):

```rust
    }
}

/// Prints events as they arrive. Returns the last assistant text and whether an action was blocked.
///
/// If stdout is closed (e.g. the reader end of a pipe exits early), writing must not panic: it sets
/// `stdout_broken` and cancels the run so it stops promptly, but keeps draining events (so `blocked`
```

with:

```rust
    }
}

/// Opens `<state>/logs/<run_id>.log` for `--debug`, readable only by its owner.
fn open_log(
    state_dir: &Path,
    run_id: &str,
) -> std::io::Result<(std::fs::File, std::path::PathBuf)> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let dir = state_dir.join("logs");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    let path = dir.join(format!("{run_id}.log"));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    Ok((file, path))
}

/// Prints events as they arrive, and writes them to the debug `log`, with every secret
/// `redactor` knows replaced. Returns the last assistant text and whether an action was blocked.
///
/// If stdout is closed (e.g. the reader end of a pipe exits early), writing must not panic: it sets
/// `stdout_broken` and cancels the run so it stops promptly, but keeps draining events (so `blocked`
```

Replace (6 of 7):

```rust
    mut rx: mpsc::UnboundedReceiver<AgentEvent>,
    json: bool,
    cancel: CancellationToken,
) -> (String, bool) {
    let mut last_text = String::new();
    let mut blocked = false;
```

with:

```rust
    mut rx: mpsc::UnboundedReceiver<AgentEvent>,
    json: bool,
    cancel: CancellationToken,
    redactor: Arc<Redactor>,
    mut log: Option<std::fs::File>,
) -> (String, bool) {
    let mut last_text = String::new();
    let mut blocked = false;
```

Replace (7 of 7):

```rust
    let mut writes: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut stdout_broken = false;
    while let Some(event) = rx.recv().await {
        if json && !stdout_broken {
            let line = serde_json::to_string(&event).expect("events serialize");
            if writeln!(std::io::stdout().lock(), "{line}").is_err() {
                stdout_broken = true;
                cancel.cancel();
            }
        }
        match &event {
            AgentEvent::AssistantMessage { content, .. } if !content.is_empty() => {
                last_text = content.clone()
```

with:

```rust
    let mut writes: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut stdout_broken = false;
    while let Some(event) = rx.recv().await {
        let line = redactor.redact(&serde_json::to_string(&event).expect("events serialize"));
        if let Some(file) = log.as_mut() {
            let _ = writeln!(file, "{line}");
        }
        if json && !stdout_broken && writeln!(std::io::stdout().lock(), "{line}").is_err() {
            stdout_broken = true;
            cancel.cancel();
        }
        // Everything shown below comes from the redacted event.
        let event: AgentEvent = serde_json::from_str(&line).unwrap_or(AgentEvent::Warning {
            message: "an event was left out because it could not be shown without a secret".into(),
        });
        match &event {
            AgentEvent::AssistantMessage { content, .. } if !content.is_empty() => {
                last_text = content.clone()
```

- [ ] **Step 6: Describe P4 in the README**

In `README.md`:

Replace (1 of 5):

```markdown

A terminal-first, open-source coding agent written in Rust, built for **hybrid development**: mix local models with frontier models so you get work done while saving subscription usage.

> **Status: early development.** Milestone 1, phases P1 (foundation), P2 (safety) and P3 (memory) are complete: a headless `harness ask` that runs multi-step coding tasks against any OpenAI-compatible model in a sandboxed environment, with project instructions, slash commands, saved sessions and checkpoints. More providers and the interactive terminal UI are in progress. Not ready for daily use yet.

## What works today

- `harness ask "<prompt>"`: runs one task to completion with six tools (`read`, `write`, `edit`, `bash`, `grep`, `glob`). Piped stdin is appended to the prompt; `--json` streams every event as NDJSON.
- `harness models`: lists models from local servers (Ollama, LM Studio, llama.cpp) and configured providers.
- Providers: any OpenAI-compatible endpoint. Built in: `ollama`, `lmstudio`, `llamacpp`, `openrouter` (`OPENROUTER_API_KEY`).
- Approval modes: `plan`, `read-only`, `ask`, `auto`, `full-access`. Default is `auto` inside a git repository and `ask` elsewhere.
- Exit codes for scripting: `0` success, `1` runtime error, `2` invalid usage or no model, `3` an action was blocked for lack of approval, `130` interrupted.
- Project instructions: `AGENTS.md` (or `CLAUDE.md` where a directory has no `AGENTS.md`) from `~/.config/harness/`, the repository root, and each directory down to the working directory, with `@path` import lines. They go into a system prompt that stays the same for the whole run, so model servers can reuse their prompt caches.
```

with:

```markdown

A terminal-first, open-source coding agent written in Rust, built for **hybrid development**: mix local models with frontier models so you get work done while saving subscription usage.

> **Status: early development.** Milestone 1, phases P1 (foundation), P2 (safety), P3 (memory) and P4 (providers) are complete: a headless `harness ask` that runs multi-step coding tasks in a sandboxed environment against local models, OpenAI, Anthropic, OpenRouter or a ChatGPT plan, with project instructions, slash commands, saved sessions and checkpoints. The interactive terminal UI is in progress. Not ready for daily use yet.

## What works today

- `harness ask "<prompt>"`: runs one task to completion with six tools (`read`, `write`, `edit`, `bash`, `grep`, `glob`). Piped stdin is appended to the prompt; `--json` streams every event as NDJSON.
- `harness models`: lists models from local servers (Ollama, LM Studio, llama.cpp) and from providers with a key.
- Providers: any endpoint speaking the OpenAI Chat Completions (`openai-chat`), OpenAI Responses (`openai-responses`) or Anthropic Messages (`anthropic-messages`) protocol. Built in: `ollama`, `lmstudio`, `llamacpp`, `openrouter` (`OPENROUTER_API_KEY`), `openai` (`OPENAI_API_KEY`), `anthropic` (`ANTHROPIC_API_KEY`) and `chatgpt` (signed in).
- API keys: `harness auth add <provider>` reads a key from standard input (typed without echo, or piped from a password manager) and stores it in the macOS Keychain or the Secret Service keyring on Linux, or, where there is none, in `~/.local/share/harness/credentials.json`, readable only by you. An environment variable wins over a stored key. `--profile <name>` stores more than one account; `harness auth use <provider> <profile>` picks one; `harness logout <provider>` removes one.
- ChatGPT: `harness login chatgpt` signs in with your ChatGPT account in the browser (`--device` for a code to enter on another device, as over SSH), and `--model chatgpt/<model>` then uses the models your plan includes. OpenAI allows this in third-party tools today, but that is its current practice, not a guarantee.
- Model profiles: `[profiles."<glob>"]` sets a model's context window, output limit, temperature, reasoning effort and whether tool calls it writes as text are run. Built-in profiles cover common open-weight coding models (Qwen3-Coder, Devstral, gpt-oss and others) and the hosted families. Local servers are asked what context they really run a model with, and a window too small for agentic work is warned about with the fix (`OLLAMA_CONTEXT_LENGTH`, `llama-server -c`).
- Local models: tool calls written as text (`<tool_call>` blocks, or a message that is only a call's JSON) run like native ones, and a reply cut off at the output limit never runs a partial tool call; the model is asked to continue in smaller steps.
- Secrets: API keys, sign-in tokens and environment variables whose names end in `KEY`, `TOKEN`, `SECRET` or `PASSWORD` are replaced by `[redacted]` in session files, tool-output files, `--json` output and everything harness prints. `--debug` writes the run's events, redacted too, to `~/.local/state/harness/logs/`.
- Approval modes: `plan`, `read-only`, `ask`, `auto`, `full-access`. Default is `auto` inside a git repository and `ask` elsewhere.
- Exit codes for scripting: `0` success, `1` runtime error, `2` invalid usage or no model, `3` an action was blocked for lack of approval, `130` interrupted.
- Project instructions: `AGENTS.md` (or `CLAUDE.md` where a directory has no `AGENTS.md`) from `~/.config/harness/`, the repository root, and each directory down to the working directory, with `@path` import lines. They go into a system prompt that stays the same for the whole run, so model servers can reuse their prompt caches.
```

Replace (2 of 5):

````markdown
```toml
model = "ollama/qwen3:14b"

[providers.work]
protocol = "openai-chat"
base_url = "https://llm.example.com/v1"
````

with:

````markdown
```toml
model = "ollama/qwen3:14b"

[profiles."ollama/qwen3*"]
context_window = 40960    # what the model takes; harness also asks Ollama what it runs with
temperature = 0.6

[providers.work]
protocol = "openai-chat"
base_url = "https://llm.example.com/v1"
````

Replace (3 of 5):

````markdown
keep_recent_percent = 20  # keep this share of recent messages as they are
```

Project-level `.harness/config.toml` settings that widen what the agent may do (allow rules, `read_dirs`, model, providers, sandbox settings, a `mode` wider than your global or default mode, a `max_steps` above your global limit) only apply after `harness trust`, and so does a `[compaction] threshold_percent` below 50. The same trust lets a project's command files choose their own `model`; a repository with command files and no project settings can be trusted too.

## Known limitations

````

with:

````markdown
keep_recent_percent = 20  # keep this share of recent messages as they are
```

Project-level `.harness/config.toml` settings that widen what the agent may do (allow rules, `read_dirs`, model, providers, model profiles, sandbox settings, a `mode` wider than your global or default mode, a `max_steps` above your global limit) only apply after `harness trust`, and so does a `[compaction] threshold_percent` below 50. The same trust lets a project's command files choose their own `model`; a repository with command files and no project settings can be trusted too.

## Known limitations

````

Replace (4 of 5):

```markdown
- **Hard links:** on macOS, files with more than one hard link cannot be modified inside the sandbox. On Linux, hard links that already point outside the workspace stay writable.
- **Git inside the sandbox** cannot create repositories or worktrees in the workspace (`git init`, `git clone`, `git worktree add`); on Linux they are created and then moved to the quarantine. On macOS a nested repository can still be moved out of the workspace, edited and moved back. `.git/rebase-merge/git-rebase-todo` stays writable, so a sandboxed command could add `exec` lines that run the next time you continue a rebase (`git rebase --continue`). A workspace that is a linked worktree (its `.git` file points into another repository's `.git/worktrees/`) cannot commit inside the sandbox, because that gitdir is outside the workspace. A repository outside the workspace in a writable temp or `writable_roots` directory gets no protection.
- **Path rules** are matched after resolving symlinks. On macOS, write `/private/tmp/...` rather than `/tmp/...` in `allow` rules.
- **One context window for every model.** Until model profiles arrive, harness assumes 32,768 tokens: compaction starts at 80% of that, and instruction files over a quarter of it get a warning. A server with a smaller window that rejects a long request gets one compacted retry; one that silently truncates (Ollama's default) does not.
- **Checkpoints** cover the working directory, not files over 10 MB, git-ignored files, `node_modules`, `target`, `.harness/` or a top-level `HEAD`, or what is inside nested repositories; a rewind leaves those alone, including a file that was ignored or too large when the checkpoint was taken. In a subdirectory of a repository, the repository's own ignore rules apply, unless they ignore that subdirectory itself (then only its own `.gitignore` files do); your global git excludes file does not. A checkpoint is restored only in the directory it was taken in, so a session continued from another directory can rewind its conversation but not those files. git stores only whether a file is executable: a restored file gets your umask's permissions, except files only you could read, which get theirs back. Checkpoints are off, with a warning, when harness's data directory is inside the workspace or a directory sandboxed commands can write to (a temp directory, or a `writable_roots` entry), and in a workspace whose first snapshot takes longer than 5 seconds. They need git 2.26 or later.
- **Command files run with their own settings.** A command file's `allowed-tools` pre-approve the commands it names (never beyond deny rules, destructive-command confirmation or the sandbox). Its `model` answers its invocations if the file is your own (`~/.config/harness/commands`, `~/.claude/commands`); a project's command file chooses the model only once `harness trust` has trusted the directory the command files come from, the repository root (run it there, not in a subdirectory); otherwise a note says the session's model answers instead. In a command file's `` !`…` `` commands, the arguments are shell parameters set before the command runs: write `"$1"` or `"$ARGUMENTS"` in double quotes, as in any script (an unquoted `$1` is split and globbed, and `'$1'` is the text `$1`). A command that uses the arguments with a construct where bash may evaluate a value as code or arithmetic (`$((…))`, `$[…]`, `let`, `declare`, subscripts, `=(…)`, `eval`, `trap`, `read`, `printf -v`, `source`, `.`, `${!…}`, `${…@P}`, `compgen`, `complete`, `enable`, and a few more) is not run, with a warning. That list defends ordinary command bodies; one that deliberately hands an argument to something that runs it later, such as `PS4`, `PROMPT_COMMAND` or `BASH_ENV`, is the command author's responsibility, as in any script. Read command files from repositories you did not write before running them.
- **Instruction files and command files are read when a run starts**; changes apply to the next run.
```

with:

```markdown
- **Hard links:** on macOS, files with more than one hard link cannot be modified inside the sandbox. On Linux, hard links that already point outside the workspace stay writable.
- **Git inside the sandbox** cannot create repositories or worktrees in the workspace (`git init`, `git clone`, `git worktree add`); on Linux they are created and then moved to the quarantine. On macOS a nested repository can still be moved out of the workspace, edited and moved back. `.git/rebase-merge/git-rebase-todo` stays writable, so a sandboxed command could add `exec` lines that run the next time you continue a rebase (`git rebase --continue`). A workspace that is a linked worktree (its `.git` file points into another repository's `.git/worktrees/`) cannot commit inside the sandbox, because that gitdir is outside the workspace. A repository outside the workspace in a writable temp or `writable_roots` directory gets no protection.
- **Path rules** are matched after resolving symlinks. On macOS, write `/private/tmp/...` rather than `/tmp/...` in `allow` rules.
- **Context windows.** A model no profile knows gets 8,192 tokens, with a warning that says how to set its `context_window`. Only the built-in `ollama`, `lmstudio` and `llamacpp` providers are asked what they run a model with: another server that silently truncates long requests is not noticed, and an LM Studio model that is not loaded yet counts as unknown. Asking Ollama loads the model, as the first request would.
- **ChatGPT sign-in** uses the public OAuth client of OpenAI's open-source Codex CLI, as other third-party tools do; OpenAI could stop allowing that. `harness models` does not list ChatGPT's models: use a model your plan includes as `chatgpt/<model>`. Builds made with `--no-default-features` leave sign-in out.
- **Claude subscriptions** (Free, Pro, Max) cannot be used: Anthropic allows them only in Claude Code, so harness refuses a subscription token (`sk-ant-oat…`) and needs an Anthropic API key.
- **The keychain on Linux** is the Secret Service over D-Bus (GNOME Keyring, KWallet); without one, keys go to `credentials.json` with a warning. A macOS keychain entry made by one build of harness may ask for your password once a rebuilt binary reads it.
- **Redaction** covers what harness writes, not what the model is sent: a key a command prints still reaches the model, which may repeat it in a file it writes. Values shorter than eight characters are not redacted.
- **Checkpoints** cover the working directory, not files over 10 MB, git-ignored files, `node_modules`, `target`, `.harness/` or a top-level `HEAD`, or what is inside nested repositories; a rewind leaves those alone, including a file that was ignored or too large when the checkpoint was taken. In a subdirectory of a repository, the repository's own ignore rules apply, unless they ignore that subdirectory itself (then only its own `.gitignore` files do); your global git excludes file does not. A checkpoint is restored only in the directory it was taken in, so a session continued from another directory can rewind its conversation but not those files. git stores only whether a file is executable: a restored file gets your umask's permissions, except files only you could read, which get theirs back. Checkpoints are off, with a warning, when harness's data directory is inside the workspace or a directory sandboxed commands can write to (a temp directory, or a `writable_roots` entry), and in a workspace whose first snapshot takes longer than 5 seconds. They need git 2.26 or later.
- **Command files run with their own settings.** A command file's `allowed-tools` pre-approve the commands it names (never beyond deny rules, destructive-command confirmation or the sandbox). Its `model` answers its invocations if the file is your own (`~/.config/harness/commands`, `~/.claude/commands`); a project's command file chooses the model only once `harness trust` has trusted the directory the command files come from, the repository root (run it there, not in a subdirectory); otherwise a note says the session's model answers instead. In a command file's `` !`…` `` commands, the arguments are shell parameters set before the command runs: write `"$1"` or `"$ARGUMENTS"` in double quotes, as in any script (an unquoted `$1` is split and globbed, and `'$1'` is the text `$1`). A command that uses the arguments with a construct where bash may evaluate a value as code or arithmetic (`$((…))`, `$[…]`, `let`, `declare`, subscripts, `=(…)`, `eval`, `trap`, `read`, `printf -v`, `source`, `.`, `${!…}`, `${…@P}`, `compgen`, `complete`, `enable`, and a few more) is not run, with a warning. That list defends ordinary command bodies; one that deliberately hands an argument to something that runs it later, such as `PS4`, `PROMPT_COMMAND` or `BASH_ENV`, is the command author's responsibility, as in any script. Read command files from repositories you did not write before running them.
- **Instruction files and command files are read when a run starts**; changes apply to the next run.
```

Replace (5 of 5):

```markdown

| Milestone | Scope |
|---|---|
| M1 Core agent | Phases P1 foundation, P2 safety and P3 memory (AGENTS.md, slash commands, sessions, checkpoints, compaction) done; P4 providers (ChatGPT sign-in, Anthropic, model profiles), P5 terminal UI (rewind picker, `/compact`, `/resume`) |
| M2 Routing | Model roles, boundary-based switching, usage ledger and "$ saved", verification gates |
| M3 Agents | Subagents, delegation to Claude Code and Codex, parallel agents in worktrees |
| M4 Ecosystem | Hooks, MCP, Agent Skills, ACP server |
```

with:

```markdown

| Milestone | Scope |
|---|---|
| M1 Core agent | Phases P1 foundation, P2 safety, P3 memory (AGENTS.md, slash commands, sessions, checkpoints, compaction) and P4 providers (OpenAI, Anthropic, ChatGPT sign-in, credentials, model profiles) done; P5 terminal UI (rewind picker, `/compact`, `/resume`, `/model`) |
| M2 Routing | Model roles, boundary-based switching, usage ledger and "$ saved", verification gates |
| M3 Agents | Subagents, delegation to Claude Code and Codex, parallel agents in worktrees |
| M4 Ecosystem | Hooks, MCP, Agent Skills, ACP server |
```

- [ ] **Step 7: Run everything**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo clippy -p harness-cli --no-default-features --all-targets -- -D warnings && cargo test --workspace && cargo deny check && openspec validate add-core-agent --strict`
Expected: clean; `cargo test --workspace` passes (1,135 tests on macOS); `cargo deny` reports `advisories ok, bans ok, licenses ok, sources ok`; the change is valid.

- [ ] **Step 8: Commit**

```bash
git add README.md crates/harness-core crates/harness-providers crates/harness-cli
git commit -F - <<'EOF'
feat: keep secrets out of what harness writes; docs: describe P4

API keys and sign-in tokens harness uses, and environment variables
whose names end in KEY, TOKEN, SECRET or PASSWORD, are replaced by
[redacted] in session files, tool-output files, NDJSON output and
everything harness prints; the model is still sent tool output as it
is. --debug writes the run's events, redacted, to a log in the state
directory. The README describes the providers, credentials, ChatGPT
sign-in, model profiles and their limitations.

<trailer lines from the controller>
EOF
```

- [ ] **Step 9: Leave tasks.md unticked**

The controller ticks 4.1 to 4.7 in `openspec/changes/add-core-agent/tasks.md` after the pull request's CI is green and the final review is done.

---

## Known limitations

These ship with P4; the README states the ones users meet.

- **No live verification.** Every provider, the authorization server and the local servers were mocks built from documentation and Codex's code (see "What was not verified").
- **ChatGPT sign-in depends on OpenAI's tolerance** of third-party use of Codex's OAuth client (decision 1) and of identifying to it as the Codex CLI, with Codex's own `originator` and scope list (decision 2). `--no-default-features` builds leave it out.
- **Reasoning between steps.** The Responses adapter keeps no reasoning items, so OpenAI's reasoning models start each step's reasoning afresh (decision 3); Anthropic's extended thinking is not requested.
- **Context windows** are known for the families in the built-in profiles and for what `ollama`, `lmstudio` and `llamacpp` report; any other server's model gets 8,192 tokens until a profile says otherwise, and a server that silently truncates is still not noticed. LM Studio models that are not loaded count as unknown.
- **Strict chat templates** still see two user turns when a turn that ended on tool results (step limit, Ctrl+C) is followed by a new prompt (decision 5).
- **Redaction** covers what harness writes, not what the model is sent (decision 15); values shorter than eight characters, and secrets harness does not know (a key in a file it reads), are not redacted.
- **The keychain on Linux** needs a Secret Service (GNOME Keyring, KWallet) on the session bus; otherwise keys go to `credentials.json` with a warning. A keychain entry made by one build of harness may ask for the macOS password when a rebuilt binary reads it.
- **Model lists.** `harness models` does not list ChatGPT's models (decision 18), and OpenAI's `/models` lists every model the key can reach, not only chat models.

## Roadmap

| Phase | What builds on this |
|---|---|
| P5 Terminal UI | The first-run model picker lists credentialed providers (`configured_endpoints`) and signs in with `login::run`; `/model` switches between adapters mid-session (P3's `TurnModel`); the status line shows `Window::tokens` and the profile; interactive approval can ask before a local conversation goes hosted (decision 14); `/usage` can show `Usage::cached_tokens` from the new adapters |
| M2 Routing | Model roles resolve profiles per role; exhausted quotas (`is_quota_exhausted`, `resets_at`) are the router's signal to switch |
| M3 Agents | Delegation to `codex app-server` as a sanctioned alternative to ChatGPT sign-in |

## Plan Completion Checklist

- [ ] `cargo test --workspace` is green on macOS, and the pull request's CI is green on every job (`macos-latest` and the `ubuntu-24.04` jobs).
- [ ] `cargo deny check` passes, and `cargo clippy -p harness-cli --no-default-features --all-targets -- -D warnings` is clean.
- [ ] With an OpenAI key: `pass show openai | harness auth add openai` stores it; `harness --model openai/<model> ask "hi"` answers without `OPENAI_API_KEY` set.
- [ ] With an Anthropic key: `harness --model anthropic/<model> ask "list the files here"` runs a tool call and answers.
- [ ] `harness login chatgpt` signs in in the browser and `harness --model chatgpt/<model> ask "hi"` answers; `harness login chatgpt --device` works over SSH.
- [ ] With Ollama running a model at its default context, `harness --model ollama/<model> ask "hi"` warns about the 4,096-token window and names `OLLAMA_CONTEXT_LENGTH`.
- [ ] Tick 4.1 to 4.7 in `tasks.md` (the controller).
