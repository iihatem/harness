## Purpose

Permissions and sandboxing protect the user's machine by controlling which actions need approval, confining shell commands to the workspace without network access by default, and keeping approval prompts rare enough to be read.

## ADDED Requirements

### Requirement: Five approval modes
The system SHALL support the approval modes `plan`, `read-only`, `ask`, `auto`, and `full-access`. In `plan` and `read-only`, file writes MUST be rejected and shell commands MUST run with a read-only filesystem. In `ask`, every file write and shell command MUST require approval. In `auto`, file writes inside the workspace and sandboxed shell commands MUST proceed without approval. In `full-access`, actions MUST proceed without approval or sandbox, except for deny rules.

#### Scenario: Auto mode edit inside the workspace
- **WHEN** the model edits a file inside the workspace in `auto` mode
- **THEN** the edit is applied without an approval prompt

#### Scenario: Read-only mode write
- **WHEN** the model calls `write` in `read-only` mode
- **THEN** the model receives an error result and no file is written

### Requirement: Default mode depends on version control
The system SHALL default to `auto` when the workspace is inside a git work tree and to `ask` otherwise, unless configuration or `--mode` specifies a mode. `full-access` MUST only be enabled by an explicit flag or trusted configuration value, and the interface MUST show a persistent warning while it is active.

#### Scenario: Directory without git
- **WHEN** harness starts in a directory that is not inside a git work tree and no mode is configured
- **THEN** the session starts in `ask` mode

### Requirement: Allow and deny rules
The system SHALL evaluate configured `allow` and `deny` rules of the form `<tool>:<glob>` before prompting. A matching deny rule MUST block the action in every mode, including `full-access`, and MUST take precedence over any allow rule. A matching allow rule MUST let the action proceed without a prompt, subject to the sandbox and destructive-command confirmation.

#### Scenario: Deny beats allow
- **WHEN** `allow = ["bash:git *"]` and `deny = ["bash:git push*"]` and the model runs `git push`
- **THEN** the command is blocked and the model receives an error result naming the deny rule

### Requirement: Compound shell commands are checked part by part
The system SHALL decompose shell commands into sub-commands across sequences, `&&`, `||`, pipes, subshells, and command substitutions before evaluating rules. A command MUST run without a prompt only if every sub-command is allowed, and MUST be blocked if any sub-command matches a deny rule. A command that cannot be fully decomposed MUST require approval.

#### Scenario: Allow-listed prefix chained with a denied command
- **WHEN** `allow = ["bash:cargo test*"]`, `deny = ["bash:curl*"]`, and the model runs `cargo test && curl https://example.com`
- **THEN** the command is blocked

#### Scenario: Command substitution
- **WHEN** `allow = ["bash:echo*"]` and the model runs `echo $(rm -rf src)` in `ask` mode
- **THEN** the user is asked to approve, because `rm -rf src` is not allowed

### Requirement: Destructive commands always need confirmation
Outside `full-access`, the system SHALL require approval for destructive commands even in `auto` mode and even when an allow rule matches: forced `git push`, `git reset --hard`, `git clean` with a force flag, discarding all working-tree changes with `git checkout -- .` or `git restore .`, recursive `rm` targeting the workspace root or a path outside the workspace, and any user-configured `confirm` patterns.

#### Scenario: Hard reset in auto mode
- **WHEN** the model runs `git reset --hard HEAD~3` in `auto` mode
- **THEN** the user is asked to approve before the command runs

### Requirement: Reading outside the workspace needs approval
Except in `full-access`, the system SHALL require approval for `read`, `grep`, and `glob` on paths outside the workspace, other than the harness tool-output directory and configured `read_dirs`.

#### Scenario: Reading credentials in the home directory
- **WHEN** the model reads `~/.aws/credentials` in `auto` mode
- **THEN** the user is asked to approve before the file is read

### Requirement: Approval prompts offer once, session, or deny with feedback
When approval is required interactively, the system SHALL offer: approve once; approve for the rest of the session for the same tool and command prefix or path pattern; or deny with an optional message returned to the model. Session approvals MUST NOT apply to destructive commands. Prompts for `write` and `edit` MUST show the diff.

#### Scenario: Approve for session
- **WHEN** the user approves `cargo test` for the session and the model later runs `cargo test --all`
- **THEN** the second command runs without a prompt

### Requirement: Non-interactive runs deny actions that need approval
When no user can answer an approval prompt, the system SHALL deny the action, tell the model it was denied for lack of approval, and record that an action was blocked.

#### Scenario: Headless write in ask mode
- **WHEN** `harness ask --mode ask` needs to write a file
- **THEN** the write is denied, the model is informed, and the process exits with code 3

### Requirement: Shell commands run in an OS sandbox
Except in `full-access` mode, the system SHALL run `bash` commands and command-file shell expansions inside an OS sandbox (Seatbelt on macOS; Landlock and seccomp on Linux, or bubblewrap when installed) that permits writes only to the workspace and temporary directories (none in `plan` and `read-only`) and denies outbound network access. When a command fails because of a sandbox denial, the interactive interface MUST offer to re-run it without the sandbox, subject to approval.

#### Scenario: Write outside the workspace
- **WHEN** a sandboxed command runs `touch ../outside.txt`
- **THEN** the command fails and `../outside.txt` does not exist

#### Scenario: Network blocked
- **WHEN** a sandboxed command runs `curl https://example.com` in `auto` mode
- **THEN** the connection fails and the user is offered an approval to re-run it without the sandbox

### Requirement: No silent unsandboxed fallback
When no sandbox mechanism is available, the system SHALL warn the user at startup and require approval for every `bash` command in every mode except `full-access`. The system MUST NOT run a command unsandboxed without either `full-access` or an explicit approval.

#### Scenario: Linux kernel without Landlock
- **WHEN** harness starts on a system where neither Landlock nor bubblewrap is available, in `auto` mode
- **THEN** a warning is shown and each `bash` command asks for approval

### Requirement: File tools enforce the workspace boundary
The system SHALL resolve `write` and `edit` target paths to canonical absolute paths, following symbolic links, before checking permissions. Targets outside the workspace MUST require approval in every mode except `full-access`, and MUST be rejected in `plan` and `read-only`.

#### Scenario: Symlink escape
- **WHEN** the model edits `link/file.txt` where `link` is a symlink to a directory outside the workspace, in `auto` mode
- **THEN** the edit requires approval
