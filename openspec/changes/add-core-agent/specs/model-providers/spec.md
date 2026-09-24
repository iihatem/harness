## Purpose

Model providers connect the runtime to language models over standard wire protocols, covering hosted APIs, subscription sign-in, and local model servers, while keeping conversations portable between models and making local models reliable.

## ADDED Requirements

### Requirement: Three wire protocols are supported
The system SHALL support streaming model calls with tool use over the OpenAI Chat Completions protocol, the OpenAI Responses protocol, and the Anthropic Messages protocol. Any provider MUST be configurable by choosing one of these protocols and a base URL.

#### Scenario: Custom OpenAI-compatible endpoint
- **WHEN** the user configures a provider with protocol `openai-chat`, a base URL, and an API key environment variable
- **THEN** models from that provider can be selected and used for turns with tool calls

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
The system SHALL store conversation history in a provider-neutral form so that the model can be changed mid-session without losing history. Provider-specific content the new provider cannot accept MUST be omitted rather than causing an error.

#### Scenario: Switching from a local model to GPT
- **WHEN** the user switches from `ollama/<model>` to `chatgpt/<model>` after several turns
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
When text tool-call parsing is enabled for the active model (the default for local providers), the system SHALL treat an assistant message that contains no native tool calls and consists of `<tool_call>` blocks, or solely of a JSON object with `name` and `arguments` fields, as tool calls. Recovered calls MUST go through the same validation and permission checks as native calls. Text that merely contains such structures alongside other prose MUST NOT be treated as a tool call.

#### Scenario: Local model emits a tagged tool call as text
- **WHEN** a local model replies only with `<tool_call>{"name":"read","arguments":{"path":"src/lib.rs"}}</tool_call>`
- **THEN** the `read` tool runs on `src/lib.rs`

#### Scenario: Example code in prose
- **WHEN** a reply explains the tool format and includes a JSON example inside a longer paragraph
- **THEN** no tool call is executed

### Requirement: Truncated output is detected
When the provider reports that output stopped because it reached the output-token limit, the system SHALL NOT execute any partial tool call from that output, and MUST tell the model its output was cut off and ask it to continue in smaller steps.

#### Scenario: Write call cut off
- **WHEN** a `write` call's arguments are cut off by the output limit
- **THEN** no file is written and the model receives a message that its output was truncated

### Requirement: The user chooses a default model on first use
When no model is configured or given on the command line, interactive mode SHALL show the model picker listing discovered and credentialed models and save the selection as the global default. If no models are available, interactive mode MUST guide the user to sign in or configure a provider. `harness ask` MUST NOT pick a model implicitly: it MUST exit with code 2 and a message listing any available models and how to set a default.

#### Scenario: First interactive run with Ollama running
- **WHEN** no model is configured and Ollama serves two models
- **THEN** the model picker lists both, and the chosen model is saved to the global configuration file

#### Scenario: Headless without a configured model
- **WHEN** no model is configured and `harness ask "hi"` is run
- **THEN** the process prints the available models and how to set a default, and exits with code 2
