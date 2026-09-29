## Purpose

Model providers connect the runtime to language models over standard wire protocols, covering hosted APIs, subscription sign-in, and local model servers, while keeping conversations portable between models and making local models reliable.

## ADDED Requirements

### Requirement: Three wire protocols are supported
The system SHALL support streaming model calls with tool use over the OpenAI Chat Completions protocol, the OpenAI Responses protocol, and the Anthropic Messages protocol. Any provider MUST be configurable by choosing one of these protocols and a base URL.

#### Scenario: Custom OpenAI-compatible endpoint
- **WHEN** the user configures a provider with protocol `openai-chat`, a base URL, and an API key environment variable
- **THEN** models from that provider can be selected and used for turns with tool calls

### Requirement: Errors inside a response stream are treated as their HTTP forms
The system SHALL classify an error a provider reports inside a response stream as the HTTP error it stands for, so that it is retried, or not, as that error would be. Anthropic's overloaded, API and rate-limit errors MUST be treated as HTTP 529, 500 and 429. OpenAI's error codes MUST be classified as the Codex CLI classifies them: `server_is_overloaded` as HTTP 503 and `rate_limit_exceeded` and `slow_down` as HTTP 429, waiting as long as the message asks ("try again in N s"); an exhausted quota or plan limit as a quota error, which is not retried and names the reset time when given; a context overflow and refused prompts as final; and any other code, or none, as a retryable server error. An `error` event whose details are nested under `error` MUST keep its code.

#### Scenario: OpenAI overloaded mid-stream
- **WHEN** a Responses stream fails with `server_is_overloaded` before any output
- **THEN** the request is retried like an HTTP 503

#### Scenario: Quota exhausted mid-stream
- **WHEN** a Responses stream fails with `insufficient_quota`
- **THEN** the request is not retried, and the error says the usage limit is reached

### Requirement: A stalled response stream ends
The system SHALL end a response that sends no data within 300 seconds of the request, or within 30 minutes when the model's profile says it is local (a slash command's model included), and a response that sends nothing for 300 seconds after its first data, with an error that says so. A local server that sends no data within its 30 minutes MUST NOT be asked again, since a retry would start over what it was doing, and the error MUST say what to check; a hosted provider that sends none within 300 seconds MUST be asked once more at most; silence after the first data MUST be retried like other network errors. Only silence MUST count, not the length of the whole reply, and any data MUST count as activity. The user MUST be able to end any such wait with Ctrl+C. Network errors MUST show a request URL's host and path only, never its query, fragment or credentials.

#### Scenario: Server goes silent mid-reply
- **WHEN** a provider stops sending in the middle of a reply without closing the connection
- **THEN** after 300 seconds without data the request fails with a retryable network error, and a headless run does not hang

#### Scenario: A local server reads a long prompt
- **WHEN** a local server sends its headers and then takes ten minutes to read the prompt before its first token
- **THEN** the reply is waited for, since a local server gets 30 minutes to start it, and Ctrl+C ends the wait at once

#### Scenario: A local server never starts its reply
- **WHEN** a local server sends nothing for 30 minutes after a request, in a headless run
- **THEN** the turn fails without asking the server again, with an error that says the local server did not start its reply and what to check

### Requirement: Reasoning summaries are streamed
The system SHALL ask OpenAI's reasoning models for reasoning summaries (`summary: "auto"`) on the Responses protocol, keeping the API's default reasoning effort unless the model's profile sets one, and stream the summaries as reasoning. Reasoning models MUST include the `gpt-5*` family except `gpt-5-chat*`, the `o1`, `o3` and `o4` series, every model on ChatGPT's backend, and any model whose profile sets a reasoning effort. When the API refuses summaries (as it does to an organization it has not verified), the request MUST be sent again without them, and they MUST NOT be asked for again in that session. A refusal MUST reach the user as the reply's text.

#### Scenario: GPT-5 without a profile effort
- **WHEN** a turn runs on `openai/gpt-5` and no profile sets `reasoning_effort`
- **THEN** the request asks for reasoning summaries, and they are shown while the model reasons

### Requirement: Models are identified as provider/model
The system SHALL identify models as `<provider>/<model>`, where `<provider>` is a built-in or configured provider name. Built-in providers MUST include `ollama`, `lmstudio`, `llamacpp`, `openai`, `anthropic`, `openrouter`, and `chatgpt`.

#### Scenario: Selecting a model by id
- **WHEN** the user runs `harness --model ollama/qwen3-coder:30b`
- **THEN** the session uses that model on the Ollama provider

### Requirement: Local model servers are discovered automatically
The system SHALL probe localhost for Ollama (port 11434), LM Studio (port 1234), and llama.cpp server (port 8080) and list their models without any configuration. Each probe MUST time out within 300 ms so unavailable servers do not delay startup.

#### Scenario: Ollama running
- **WHEN** Ollama is serving models and the user runs `harness models`
- **THEN** the output lists `ollama/<name>` for each served model

#### Scenario: No local servers
- **WHEN** no local server is running
- **THEN** `harness models` lists only configured and credentialed providers and reports no error

### Requirement: Conversations are portable between models
The system SHALL store conversation history in a provider-neutral form so that the model can be changed mid-session without losing history. Provider-specific content the new provider cannot accept MUST be omitted rather than causing an error. On the Anthropic Messages protocol, text that is only whitespace MUST be left out, and every tool-call id MUST go through one deterministic mapping, for calls and results alike, that replaces characters outside `[a-zA-Z0-9_-]` with `_` and appends a short hash of the original when anything was replaced, so that no two ids become one.

#### Scenario: Switching from a local model to GPT
- **WHEN** the user switches from `ollama/<model>` to `chatgpt/<model>` after several turns
- **THEN** the next request to the new model includes the prior user messages, assistant replies, and tool results

#### Scenario: Switching to Claude after another provider's tool calls
- **WHEN** the conversation holds tool calls with ids like `functions.read:0` and assistant text that is only `"\n\n"`, and the user switches to `anthropic/<model>`
- **THEN** the request carries the calls and their results under ids the Messages API accepts, without the whitespace-only text

### Requirement: Model profiles tune behaviour per model
The system SHALL resolve a model profile for the active model from user configuration, then built-in profiles, then protocol defaults, matching profile keys as globs against model ids without regard to case. Each setting MUST be resolved on its own, and within one layer the matching key with the most characters other than `*` and `?` MUST win. A profile MUST be able to set the context window, minimum context, maximum output tokens, temperature, reasoning effort, text tool-call parsing, and whether the model is local. The system MUST ship built-in profiles for common open-weight coding model families. Profiles in a project's configuration MUST apply only in a trusted workspace.

#### Scenario: User profile overrides built-in
- **WHEN** a built-in profile sets temperature 0.7 for `ollama/qwen3-coder*` and the user's config sets temperature 0.2 for the same glob
- **THEN** requests to `ollama/qwen3-coder:30b` use temperature 0.2

#### Scenario: The most specific key wins
- **WHEN** the user's config sets `context_window = 65536` for `ollama/*` and `context_window = 16384` for `ollama/qwen3-coder*`
- **THEN** `ollama/qwen3-coder:30b` uses a 16,384-token window and `ollama/llama3.1` a 65,536-token window

### Requirement: Effective context is detected and checked
The system SHALL determine a model's effective context window as the smaller of the size the serving local server reports it is actually running with and the profile's context window, fall back to 8192 tokens with a warning when neither is known, and warn the user with a remediation hint when the effective window is below the profile's minimum context (default 32,768 tokens). It MUST ask llama.cpp's `/props`, Ollama's running models (loading the model first when it is not loaded) and LM Studio's loaded model instances, and a server that does not answer in time MUST count as not reporting. When Ollama refuses to load the model (it has not been pulled, or does not fit) or is not running, the system MUST NOT warn about the window: the request that follows reports the failure, as the one message the user sees.

#### Scenario: Ollama running with a small context
- **WHEN** Ollama reports the loaded model runs with a 4,096-token context and the profile's minimum is 32,768
- **THEN** the user is warned and told how to raise the context length (e.g. `OLLAMA_CONTEXT_LENGTH`)
- **AND** context budgets use 4,096 tokens

#### Scenario: Unknown context window
- **WHEN** a model's context window is neither reported by the server nor set in any profile
- **THEN** the system uses 8192 tokens and warns the user once per session

### Requirement: Tool calls are validated before execution
The system SHALL validate each tool call's arguments against the tool's JSON schema before execution. Invalid or unparseable calls MUST NOT be executed; the model MUST receive an error result describing the problem, and the number of invalid calls per turn MUST be recorded.

#### Scenario: Malformed arguments
- **WHEN** the model emits a `read` call whose arguments are not valid JSON
- **THEN** no file is read, the model receives an error result, and the turn's invalid-call count increases by one

### Requirement: Tool calls written as text are recovered
When text tool-call parsing is enabled for the active model (the default for local providers), the system SHALL treat an assistant message that contains no native tool calls and consists of `<tool_call>` blocks, or solely of a JSON object with `name` and `arguments` (or `parameters`) fields, naming available tools, as tool calls. A block MUST be accepted holding such a JSON object, or one function in Qwen3-Coder's form (`<function=NAME>`, then `<parameter=ARG>value</parameter>` for each argument, then `</function>`), whose values are their text for parameters the tool's schema types as `string`, and otherwise JSON when they parse as JSON and their text when not. Recovered calls MUST go through the same validation and permission checks as native calls. Text that merely contains such structures alongside other prose MUST NOT be treated as a tool call. Text recovered as tool calls MUST NOT also be streamed to the user as the reply's text.

#### Scenario: Local model emits a tagged tool call as text
- **WHEN** a local model replies only with `<tool_call>{"name":"read","arguments":{"path":"src/lib.rs"}}</tool_call>`
- **THEN** the `read` tool runs on `src/lib.rs`

#### Scenario: Qwen3-Coder's own form
- **WHEN** a local model replies only with `<tool_call>` `<function=read>` `<parameter=path>` `src/lib.rs` `</parameter>` `</function>` `</tool_call>`, one tag per line
- **THEN** the `read` tool runs on `src/lib.rs`

#### Scenario: Qwen3-Coder writes a JSON file
- **WHEN** a local model writes a `write` call in Qwen3-Coder's form whose `content` parameter is a JSON object
- **THEN** the file is written with that text, since `content` is a string parameter

#### Scenario: Example code in prose
- **WHEN** a reply explains the tool format and includes a JSON example inside a longer paragraph
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

#### Scenario: First interactive run with Ollama running
- **WHEN** no model is configured and Ollama serves two models
- **THEN** the model picker lists both, and the chosen model is saved to the global configuration file

#### Scenario: Headless without a configured model
- **WHEN** no model is configured and `harness ask "hi"` is run
- **THEN** the process prints the available models and how to set a default, and exits with code 2
