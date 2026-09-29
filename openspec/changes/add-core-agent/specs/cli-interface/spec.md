## Purpose

The command-line interface presents the agent to users, both as an interactive inline terminal session and as a scriptable headless command.

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
- **THEN** the status line shows the new model and the recalculated context percentage

#### Scenario: Turn stats from a local model
- **WHEN** a turn completes on a local model that reports cached prompt tokens
- **THEN** a stats line shows the model, time to first token, tokens per second, and cache hit rate

### Requirement: Keyboard interaction
Interactive mode SHALL support: Esc to interrupt the running turn; Esc twice on empty input to open `/rewind`; Ctrl+C pressed twice within 2 seconds to exit; Shift+Tab to cycle approval modes; Alt+Enter or Shift+Enter to insert a newline, with Ctrl+J and a backslash before Enter as fallbacks for terminals that do not report Shift+Enter; Up arrow to recall previous inputs; `/` at the start of input for command completion; `@` for fuzzy completion of workspace file paths; Enter while a turn is running to queue input; and Ctrl+S while a turn is running to send input immediately (steering). An approval prompt MUST take y (approve once), a (approve for the session, when offered), n (deny, with an optional reason for the model) and Esc (deny and stop the turn).

#### Scenario: File completion
- **WHEN** the user types `@mainrs`
- **THEN** a completion list offers matching paths such as `src/main.rs`

#### Scenario: Steering key
- **WHEN** the user types a message and presses Ctrl+S while a tool is running
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
- **THEN** the input shows `[Pasted text #1, 200 lines]` and the model receives all 200 lines

### Requirement: Headless ask command
The system SHALL provide `harness ask "<prompt>"`, which runs one prompt to completion without interaction. Piped standard input MUST be appended to the prompt. By default the final assistant text MUST be written to stdout and progress to stderr; with `--json`, every runtime event MUST be written to stdout as one JSON object per line.

#### Scenario: Piping a diff
- **WHEN** the user runs `git diff | harness ask "review this"`
- **THEN** the model receives the prompt followed by the diff, and only the review text is written to stdout

#### Scenario: NDJSON output
- **WHEN** the user runs `harness ask --json "list files"`
- **THEN** each line of stdout is a valid JSON object representing one event, ending with a turn-finished event

### Requirement: Documented exit codes
`harness ask` SHALL exit with `0` on success, `1` on runtime error, `2` on invalid usage or when no usable model exists, `3` when the run completed but at least one action was blocked for lack of approval, and `130` when interrupted.

#### Scenario: Blocked action
- **WHEN** a headless run in `ask` mode is denied a file write
- **THEN** the process exits with code 3

### Requirement: Terminal-friendly output
The system SHALL honour `NO_COLOR` and MUST NOT emit ANSI escape sequences when stdout is not a terminal.

#### Scenario: Redirected output
- **WHEN** the user runs `harness ask "hi" > out.txt`
- **THEN** `out.txt` contains no ANSI escape sequences

### Requirement: Sandbox diagnosis
The system SHALL provide `harness sandbox doctor`. It reports:

- which sandbox mechanism is active;
- on Linux, which git-protection tier the session gets and why;
- the exact commands that would enable the full tier, such as an AppArmor profile for the harness binary or the user-namespace sysctl.

It MUST NOT change system files itself.

#### Scenario: Blocked user namespaces
- **WHEN** the user runs `harness sandbox doctor` on a Linux system where unprivileged user namespaces are blocked
- **THEN** it reports the basic tier, the reason, and the commands that would enable the full tier

### Requirement: Management subcommands
The system SHALL provide `harness models`, `harness login <provider>`, `harness logout <provider>`, `harness auth add <provider>`, `harness auth use <provider> <profile>`, `harness trust [--yes] [--revoke]`, and `harness sandbox doctor`, with `--profile` accepted by `login`, `logout`, and `auth add`, `--device` accepted by `login`, and the flags `--model`, `--mode`, `-c`, `--resume`, and `--debug`. `harness auth add` MUST read the key from standard input. `--debug` MUST make `harness ask` write the run's event stream, after the warnings it printed before the agent started, to a log file in the state directory that only the user can read, and print its path; other subcommands MUST refuse it.

#### Scenario: Help output
- **WHEN** the user runs `harness --help`
- **THEN** each subcommand and flag above is listed with a description
