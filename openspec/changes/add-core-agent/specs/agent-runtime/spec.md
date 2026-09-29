## Purpose

The agent runtime drives a conversation turn: it calls the model, executes requested tools, feeds results back, and reports everything that happens as a stream of typed events that every frontend consumes.

## ADDED Requirements

### Requirement: Turns are reported as an ordered event stream
The runtime SHALL report each turn as an ordered stream of typed events covering turn start, text and reasoning deltas, tool call requests, approval requests, tool results, token usage, per-turn statistics, retries, compaction, checkpoint creation, turn completion with a reason, and errors. All frontends MUST consume this same stream.

#### Scenario: Tool-using turn
- **WHEN** the model requests a tool call during a turn
- **THEN** the stream contains a tool-call-requested event before the tool-call-finished event with the same call id
- **AND** the stream ends with a turn-finished event

#### Scenario: Headless and interactive parity
- **WHEN** the same scripted model responses are replayed through the interactive frontend and through `harness ask --json`
- **THEN** both frontends receive the same sequence of event types

### Requirement: Every assistant message is attributed to its model
The runtime SHALL record, for every assistant message, the id of the model that produced it, and MUST include it in the events that carry that message. The runtime MUST NOT change the active model except in response to a user action.

#### Scenario: Attribution after a switch
- **WHEN** the user switches from `ollama/<model-a>` to `chatgpt/<model-b>` between turns
- **THEN** earlier assistant messages remain attributed to `ollama/<model-a>` and new ones to `chatgpt/<model-b>`

### Requirement: The loop runs until the model stops or a step limit is reached
The runtime SHALL execute requested tool calls and send their results back to the model until the model responds without tool calls. The runtime MUST stop after a configurable maximum number of model calls per turn (default 50) and finish the turn with reason `step_limit`.

#### Scenario: Multi-step task
- **WHEN** the model requests `read`, then `edit`, then `bash`, then replies with text only
- **THEN** all three tools run in order and the turn finishes with reason `completed`

#### Scenario: Step limit
- **WHEN** the model keeps requesting tool calls beyond the configured limit
- **THEN** the runtime stops calling the model and finishes the turn with reason `step_limit`

### Requirement: User input during a turn is queued or steered
The runtime SHALL accept user input while a turn is running. Input marked as queued MUST be delivered as the next user message after the turn finishes. Input marked as send-now MUST be delivered to the model at the next tool-result boundary within the running turn, and reported as a steered event; send-now input that no tool-result boundary took before the turn ended MUST be delivered as the next user message, before queued input. When the turn is interrupted, input not yet delivered MUST NOT be sent: the interactive frontend returns it to the input editor.

#### Scenario: Queued message
- **WHEN** the user submits "also update the README" as queued input while the agent is running tests
- **THEN** the message is sent as a new user turn after the current turn finishes

#### Scenario: Steering mid-turn
- **WHEN** the user submits "use the v2 API instead" as send-now input while a tool call is running
- **THEN** the message is included with the next tool result sent to the model in the same turn

#### Scenario: Send-now input after the last tool call
- **WHEN** the user submits send-now input while the model writes a final answer that calls no tools
- **THEN** the input is sent as the next user turn once that turn finishes

### Requirement: Turns can be interrupted
The runtime SHALL accept an interrupt at any point in a turn. On interrupt it MUST cancel the in-flight model request, terminate any running tool process including its child processes, keep the partial output already received, and finish the turn with reason `interrupted`.

#### Scenario: Interrupt during a long shell command
- **WHEN** the user interrupts while `bash` is running `sleep 600`
- **THEN** the process and its children are terminated within 2 seconds
- **AND** the turn finishes with reason `interrupted` and the session remains usable

### Requirement: Transient provider errors are retried
The runtime SHALL retry model requests that fail with network errors, HTTP 429, or HTTP 5xx, using exponential backoff with jitter, honouring a `Retry-After` header when present, for up to 5 attempts. Each retry MUST emit a retrying event with the attempt number, reason, and delay.

#### Scenario: Rate limited with Retry-After
- **WHEN** the provider responds 429 with `Retry-After: 3` and then succeeds
- **THEN** the runtime waits at least 3 seconds, emits one retrying event, and completes the turn

#### Scenario: Retries exhausted
- **WHEN** all 5 attempts fail with HTTP 503
- **THEN** the runtime emits an error event, finishes the turn with reason `error`, and the session accepts new input

### Requirement: Non-retryable provider errors are surfaced without ending the session
The runtime SHALL surface authentication failures, subscription or quota exhaustion, and other 4xx responses as error events with a human-readable message, including the reset time when the provider supplies one. The session MUST remain usable afterwards.

#### Scenario: Subscription limit reached
- **WHEN** the ChatGPT provider reports that the usage limit is reached with a reset time
- **THEN** the error event includes the reset time and suggests switching models with `/model`

### Requirement: Tool failures are returned to the model
The runtime SHALL return tool failures (invalid arguments, missing files, non-zero exit codes, timeouts, permission denials) to the model as tool results marked as errors. A tool failure MUST NOT abort the turn.

#### Scenario: Reading a missing file
- **WHEN** the model calls `read` on a path that does not exist
- **THEN** the model receives an error tool result naming the missing path and the turn continues

### Requirement: No telemetry
The system MUST NOT send usage data, analytics, crash reports, or any other telemetry. Network connections made by harness itself (excluding commands run through tools) MUST be limited to configured or built-in model providers, their authentication endpoints, and localhost discovery probes.

#### Scenario: Offline local session
- **WHEN** a session uses only a local Ollama model and the machine has no other network access
- **THEN** the session works fully and no connection attempt to any non-localhost host is made
