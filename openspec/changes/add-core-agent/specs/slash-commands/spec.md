## Purpose

Slash commands let users control the harness and run reusable prompts, including Markdown commands written for other coding agents.

## ADDED Requirements

### Requirement: Built-in commands
The system SHALL provide the built-in commands `/help`, `/model`, `/mode`, `/new`, `/resume`, `/rewind`, `/compact`, `/context`, `/usage`, `/login`, `/init`, and `/quit`. `/help` MUST list built-in and custom commands with their descriptions.

#### Scenario: Switching model
- **WHEN** the user runs `/model` and selects a different model
- **THEN** subsequent turns in the same session use the selected model

#### Scenario: Usage
- **WHEN** the user runs `/usage`
- **THEN** the input, output, and cached token counts for the current session are shown per model

### Requirement: /context shows where the context goes
The `/context` command SHALL show the active model's effective context window and a breakdown of current usage into system prompt, tool definitions, instruction files (per file), conversation history, and free space, in tokens and as percentages.

#### Scenario: Inspecting context
- **WHEN** the user runs `/context` in a session with a 2,000-token `AGENTS.md`
- **THEN** the breakdown lists `AGENTS.md` with approximately 2,000 tokens and the remaining free space

### Requirement: Custom commands are discovered from compatible locations
The system SHALL load Markdown command files from `.harness/commands/`, `.claude/commands/`, and `.opencode/commands/` in the project and from the `commands/` directory in the harness configuration directory and `~/.claude/commands/` globally, in that precedence order, with the first definition of a name winning. Subdirectories MUST become colon-separated namespaces. A custom command with the same name as a built-in MUST be ignored with a warning. A project command file or commands directory that resolves, through symlinks, outside the project MUST be skipped with a warning; global command files MAY link anywhere.

#### Scenario: Namespaced OpenSpec command
- **WHEN** the project contains `.claude/commands/opsx/propose.md`
- **THEN** `/opsx:propose` is available and listed in `/help`

#### Scenario: Name collision with a built-in
- **WHEN** the project contains `.claude/commands/help.md`
- **THEN** `/help` remains the built-in and a warning is shown

#### Scenario: A project command file linking to a secret
- **WHEN** a cloned repository's `.claude/commands/x.md` is a symlink to `~/.ssh/id_rsa`
- **THEN** `/x` is not available, a warning names the file, and nothing from it reaches the model

#### Scenario: A global command file kept in a dotfiles repository
- **WHEN** `~/.claude/commands/mine.md` is a symlink to `~/dotfiles/mine.md`
- **THEN** `/mine` is available

### Requirement: Supported frontmatter
The system SHALL honour the frontmatter fields `description`, `argument-hint`, `model`, and `allowed-tools`, and ignore unknown fields. `model` MUST apply only to that invocation, and a project command file's `model` MUST apply only when the directory the project's command files come from (the repository root, or the working directory outside a repository) is trusted, as `harness trust` run there records it. `allowed-tools` MUST map Claude Code tool names (`Bash`, `Write`, `Edit`) and patterns such as `Bash(openspec:*)` to allow rules for that invocation only, and MUST NOT override deny rules, destructive-command confirmation, or the sandbox. `Read`, `Grep`, and `Glob` MUST NOT widen reads outside the workspace.

#### Scenario: A project command's model in an untrusted workspace
- **WHEN** a project command file declares `model: other/model` and the workspace is not trusted
- **THEN** the invocation uses the session's model and a note says the command's model was ignored, naming `harness trust` and the directory to run it in

#### Scenario: A project command's model in a subdirectory
- **WHEN** the user trusted a repository's root and runs, from a subdirectory, a project command file that declares `model: other/model`
- **THEN** the invocation uses `other/model`; had the user trusted only the subdirectory, the session's model would answer, and both `harness trust` there and the note would name the repository root

#### Scenario: allowed-tools pre-approves a command
- **WHEN** a command file declares `allowed-tools: Bash(openspec:*)` and its run executes `openspec status` in `ask` mode
- **THEN** the command runs without an approval prompt, inside the sandbox

#### Scenario: allowed-tools cannot beat deny
- **WHEN** a command file declares `allowed-tools: Bash(*)` and the user's configuration denies `bash:rm -rf*`
- **THEN** `rm -rf build` during that command is blocked

### Requirement: Placeholders are expanded
The system SHALL expand `$ARGUMENTS` to the full argument string, `$1` through `$9` to positional arguments (whitespace-separated, respecting quotes), `@<path>` to the content of a workspace file, and `` !`<command>` `` to the output of a shell command. When the body uses none of the argument placeholders, non-empty arguments MUST be appended as `ARGUMENTS: <arguments>`. File references and shell commands MUST be expanded only in the command body, never in the arguments. Arguments MUST NOT be written into a shell command's text: a shell command that uses them MUST receive them as shell parameters (`$1`–`$9`, `$@`, and `$ARGUMENTS`), set by a prelude the system writes before the command, so their values reach it as data with the usual shell semantics. A shell command that uses the arguments together with a construct where the shell may evaluate a parameter's value as code, arithmetic or a variable name MUST NOT run; it MUST be left as `[not expanded: …]` in the prompt, and a warning MUST say why. Shell expansions MUST go through the same permission rules and sandbox as the `bash` tool.

#### Scenario: Positional arguments
- **WHEN** the user runs `/review "src/lib.rs" strict` for a command whose body contains `Review $1 in $2 mode`
- **THEN** the prompt sent is `Review src/lib.rs in strict mode`

#### Scenario: Arguments without a placeholder
- **WHEN** a command body contains no argument placeholder and the user runs `/opsx:propose add-login`
- **THEN** the prompt sent is the body followed by `ARGUMENTS: add-login`

#### Scenario: An argument with shell syntax reaches the command as data
- **WHEN** a command body contains `` !`git log --oneline --grep "$1"` `` and a script runs `harness ask "/review \"$TITLE\""` with a title containing `$(…)`
- **THEN** `git log` receives the title, byte for byte, as its `--grep` argument, and nothing in the title runs

#### Scenario: An argument evaluated as arithmetic
- **WHEN** a command body contains `` !`echo $[ b[0] + $1 ]` `` and the user runs it with any argument
- **THEN** the shell command does not run, the prompt holds `[not expanded: …]` in its place, and a warning names `$[`

#### Scenario: Shell expansion in ask mode
- **WHEN** a command body contains `` !`git diff` `` and the session is in `ask` mode
- **THEN** the user is asked to approve `git diff` before the command's prompt is sent

### Requirement: Commands are available headless and with completion
Custom commands and `/init` SHALL work in `harness ask`; other built-in commands and unknown commands MUST be rejected with exit code 2. In interactive mode, typing `/` at the start of input MUST show matching commands with their descriptions.

#### Scenario: Headless custom command
- **WHEN** the user runs `harness ask "/opsx:propose add-login"`
- **THEN** the command file's expanded prompt is sent as the turn's input
