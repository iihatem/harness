## Purpose

The command-line interface presents the agent to users, both as an interactive inline terminal session and as a scriptable headless command.

## ADDED Requirements

### Requirement: Interactive sessions render inline
Interactive mode SHALL render into the terminal's normal screen: completed messages MUST be written into the terminal scrollback, and only the active region (input, streaming output, prompts) MUST be redrawn. A streaming reply's Markdown blocks (a paragraph, a list, a closed code block) MUST go into the scrollback as each completes, so that redrawing costs no more as the reply grows; the active region shows the block still growing. Each line MUST reach the scrollback once, with its text whole, wide characters included, and the end of the last message MUST stay on screen above the active region; a resize of the terminal MUST NOT erase or repeat a line of it. Full-screen views MAY be used for pickers, `/rewind`, and long diffs and MUST return to inline mode when closed. Interactive mode MUST need a terminal on standard input and standard output that can move its cursor and reports its size; without one (a pipe, `TERM=dumb`), `harness` MUST exit with code 2 and name `harness ask`.

#### Scenario: Scrollback preserved
- **WHEN** a session produces more output than fits on screen
- **THEN** earlier messages remain reachable with the terminal's own scrollback

#### Scenario: A long reply streams
- **WHEN** the model streams a reply of many paragraphs and code blocks
- **THEN** each paragraph and code block enters the scrollback as it completes, highlighted once, and when the reply ends its last lines are on screen above the input

#### Scenario: Resizing the window
- **WHEN** the user makes the terminal shorter, then wider, at the prompt or while a reply streams
- **THEN** every line of the conversation is still in the scrollback or on screen exactly once, with the input below the last of them

#### Scenario: No terminal
- **WHEN** the user runs `harness` with standard input from a pipe
- **THEN** harness exits with code 2 and says to use `harness ask`

#### Scenario: A terminal that cannot move its cursor
- **WHEN** the user runs `harness` in Emacs's shell mode, where `TERM=dumb`
- **THEN** harness exits with code 2, writes no escape sequence, and says to use `harness ask`

### Requirement: Text is measured and edited by grapheme cluster
Interactive mode SHALL measure, wrap and edit text by extended grapheme cluster, as the screen draws it: an emoji with a variation selector, a ZWJ sequence and a flag are each one character two columns wide, and a letter with combining marks is one character. Wrapping MUST NOT cut text off the end of a row or split a cluster, and the input's cursor keys, Backspace and Delete MUST move over and delete a whole cluster. Control, bidirectional and invisible format characters in what is drawn MUST be shown as escapes, except joiners that an emoji or a script needs.

#### Scenario: Deleting an emoji
- **WHEN** the user types a family emoji (a ZWJ sequence) or a flag and presses Backspace once
- **THEN** the whole emoji is deleted

#### Scenario: Emoji in a reply
- **WHEN** a reply contains `⚠️` several times in a line longer than the screen
- **THEN** the line wraps with none of its text cut off

### Requirement: Status line and per-turn stats
Interactive mode SHALL display a status line showing the active model, approval mode, context usage as a percentage of the effective context window, and session token totals. After each turn it MUST show the model that answered, time to first token, output tokens per second, and prompt-cache hit rate when the provider reports cached tokens. Time to first token MUST be measured from the reply's first streamed event of any kind, including a tool call's own first fragment, not from whichever event happens to carry the reply's content; output tokens per second MUST be computed over the generation window that follows from there. The runtime MUST report these per-turn statistics as an event before the turn finishes, so `harness ask --json` prints them too.

#### Scenario: Status after switching model
- **WHEN** the user switches to a model with a larger context window
- **THEN** the status line shows the new model and the recalculated context percentage

#### Scenario: Turn stats from a local model
- **WHEN** a turn completes on a local model that reports cached prompt tokens
- **THEN** a stats line shows the model, time to first token, tokens per second, and cache hit rate

#### Scenario: Turn stats when the reply is a tool call
- **WHEN** a turn's reply is a tool call whose arguments stream for a while before the call itself arrives
- **THEN** time to first token and tokens per second are measured from the call's first fragment, not from when the whole call arrived

### Requirement: Keyboard interaction
Interactive mode SHALL support: Esc to interrupt the running turn; Esc twice on empty input to open `/rewind`; Ctrl+C pressed twice within 2 seconds to exit; Shift+Tab to cycle approval modes; Alt+Enter or Shift+Enter to insert a newline, with Ctrl+J and a backslash before Enter as fallbacks for terminals that do not report Shift+Enter; Up arrow to recall previous inputs; `/` at the start of input for command completion; `@` for fuzzy completion of workspace file paths; Enter while a turn is running to queue input; and Ctrl+S while a turn is running to send input immediately (steering). An approval prompt MUST take y (approve once), a (approve for the session, when offered), n (deny, with an optional reason for the model) and Esc (deny and stop the turn). A prompt MUST be answered only by a key pressed for it: keys typed before it was drawn, or within 300 ms after, MUST go to the input; Enter, and the letters with Ctrl or Alt held, MUST NOT answer it.

#### Scenario: A key typed ahead of a prompt
- **WHEN** the user is typing a message as an approval prompt appears, and the next key is `y` or Enter
- **THEN** the key goes to the message, and the prompt still waits for an answer

#### Scenario: File completion
- **WHEN** the user types `@mainrs`
- **THEN** a completion list offers matching paths such as `src/main.rs`

#### Scenario: File completion after the agent writes a file
- **WHEN** a turn runs a tool that creates or writes a file, and the turn ends
- **THEN** the next `@` completion offers that file

#### Scenario: Steering key
- **WHEN** the user types a message and presses Ctrl+S while a tool is running
- **THEN** the message is delivered to the model at the next tool-result boundary

### Requirement: Interactive sessions end cleanly
However an interactive session ends, it SHALL stop the running turn and the command it runs, deny any approval that waits, restore the terminal's modes, and end the sandbox's session. A hangup (SIGHUP) or SIGTERM MUST end it this way, with exit code 129 or 143; a terminal whose input ends or fails MUST end it as a hangup, whether or not harness ignores SIGHUP, and never leave harness reading it. While an external editor has the terminal, SIGINT and SIGQUIT MUST NOT end harness, and their earlier actions MUST be restored afterwards.

#### Scenario: Terminal closed during a command
- **WHEN** the user closes the terminal window while a command runs
- **THEN** the command's processes are ended, the sandbox's session ends, and harness exits with code 129

#### Scenario: Ctrl+C in the editor
- **WHEN** the user presses Ctrl+C while editing a plan in `$EDITOR`
- **THEN** harness keeps running, and its terminal modes and plan are as they were

### Requirement: Desktop notifications
Interactive mode SHALL emit an OSC 9 desktop notification and a terminal bell when a turn that ran 10 seconds or longer finishes, other than by the user's interruption, or when an approval is needed. Both MUST be configurable (`[notifications] desktop` and `bell`, on by default) and MUST be disabled when stdout is not a terminal. Text in a notification MUST have its control, bidirectional and invisible format characters removed.

#### Scenario: Long task completes
- **WHEN** a turn runs for 3 minutes and finishes
- **THEN** the terminal receives an OSC 9 notification and a bell

#### Scenario: Notifications turned off
- **WHEN** the configuration sets `[notifications] desktop = false`
- **THEN** a long turn ends with a bell and no OSC 9 notification

### Requirement: Large pastes are collapsed
Interactive mode SHALL display pasted text longer than 10 lines or 1,000 characters as a numbered placeholder showing its line count, let the user expand the placeholder to edit the text (Ctrl+O), and send the full text with the message. A paste larger than 4 MiB MUST be refused with a message, leaving the input as it was.

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
The system SHALL provide `harness models`, `harness login <provider>`, `harness logout <provider>`, `harness auth add <provider>`, `harness auth use <provider> <profile>`, `harness trust [--yes] [--revoke]`, and `harness sandbox doctor`, with `--profile` accepted by `login`, `logout`, and `auth add`, `--device` accepted by `login`, and the flags `--model`, `--mode`, `-c`, `--resume`, and `--debug`. `harness auth add` MUST read the key from standard input. `--debug` MUST make `harness ask` write the run's event stream, after the warnings it printed before the agent started, to a log file in the state directory that only the user can read, and print its path; every other subcommand, and the interactive session (no subcommand at all), MUST refuse it, since only `ask` has a run to log.

#### Scenario: Help output
- **WHEN** the user runs `harness --help`
- **THEN** each subcommand and flag above is listed with a description
