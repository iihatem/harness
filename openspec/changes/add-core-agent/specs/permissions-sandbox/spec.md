## Purpose

Permissions and sandboxing protect the user's machine by controlling which actions need approval, confining shell commands to the workspace without network access by default, and keeping approval prompts rare enough to be read.

## ADDED Requirements

### Requirement: Five approval modes
The system SHALL support the approval modes `plan`, `read-only`, `ask`, `auto`, and `full-access`. In `plan` and `read-only`, file writes MUST be rejected and shell commands MUST run with a read-only filesystem; a shell command MUST never run with write access in these modes, so when no OS sandbox is available it MUST be refused. In `ask`, every file write and shell command MUST require approval unless an allow rule matches it. In `auto`, file writes inside the workspace and sandboxed shell commands MUST proceed without approval. In `full-access`, actions MUST proceed without approval or sandbox, except for deny rules.

#### Scenario: Auto mode edit inside the workspace
- **WHEN** the model edits a file inside the workspace in `auto` mode
- **THEN** the edit is applied without an approval prompt

#### Scenario: Read-only mode write
- **WHEN** the model calls `write` in `read-only` mode
- **THEN** the model receives an error result and no file is written

#### Scenario: Plan mode without a sandbox
- **WHEN** no OS sandbox is available and the model runs `ls` in `plan` mode
- **THEN** the command is refused without running, and the model is told that shell commands need the OS sandbox in plan and read-only mode

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
The system SHALL decompose shell commands into sub-commands across sequences, `&&`, `||`, pipes, subshells, and command substitutions before evaluating rules. A command MUST run without a prompt only if every sub-command is allowed, and MUST be blocked if any sub-command matches a deny rule. A command that cannot be fully decomposed MUST require approval. A `git` command given configuration on the command line (`-c`, `--config-env`, `--exec-path`) MUST be treated as not fully decomposed, because that configuration can run programs, except for `-c` settings of keys that cannot: `user.name`, `user.email`, `init.defaultBranch`, `color.*`, `advice.*`, `core.quotepath`, and `commit.gpgsign` or `tag.gpgsign` set to `false`.

#### Scenario: Allow-listed prefix chained with a denied command
- **WHEN** `allow = ["bash:cargo test*"]`, `deny = ["bash:curl*"]`, and the model runs `cargo test && curl https://example.com`
- **THEN** the command is blocked

#### Scenario: Command substitution
- **WHEN** `allow = ["bash:echo*"]` and the model runs `echo $(rm -rf src)` in `ask` mode
- **THEN** the user is asked to approve, because `rm -rf src` is not allowed

#### Scenario: Git configuration on the command line
- **WHEN** `allow = ["bash:git *"]` and the model runs `git -c core.pager=./x log` in `auto` mode
- **THEN** the user is asked to approve
- **AND** `git -c user.name=x commit -m y` runs without a prompt

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
When approval is required interactively, the system SHALL offer: approve once; approve for the rest of the session for the same tool and command prefix or path pattern; or deny with an optional message returned to the model. Session approvals MUST NOT apply to destructive commands, and a session approval that cannot apply MUST be reported as applying once. Prompts for `write` and `edit` MUST show the diff, or, for a file that cannot be diffed, a note and the new content; building one MUST NOT wait on the target (a FIFO, a terminal) or block the session. A prompt MUST show all of what it asks about, scrolling when it does not fit, and say when part of it is hidden; characters that would draw as nothing MUST be shown as escapes. Only a key pressed for a prompt MUST answer it: until the user has paused typing for 500 ms after it appears, their keys MUST go to the message they were typing. A prompt to run a command outside the sandbox MUST NOT offer approval for the session. Once the turn is stopped, nothing more MUST be asked, and an unanswered prompt MUST be denied.

#### Scenario: A command longer than the prompt
- **WHEN** the model asks to run a one-line command longer than the prompt can show
- **THEN** the prompt says which rows show, and the user can scroll to the command's end before answering

#### Scenario: Approve for session
- **WHEN** the user approves `cargo test` for the session and the model later runs `cargo test --all`
- **THEN** the second command runs without a prompt

#### Scenario: Session approval is scoped to the command prefix
- **WHEN** the user approves `git status` for the session and the model later runs `git push`
- **THEN** `git push` still requires approval

#### Scenario: A destructive command approved for the session
- **WHEN** the user approves `git reset --hard HEAD~1` for the session
- **THEN** it runs once, the user is told the approval applied once, and the next `git reset --hard` asks again

### Requirement: Non-interactive runs deny actions that need approval
When no user can answer an approval prompt, the system SHALL deny the action, tell the model it was denied for lack of approval, and record that an action was blocked.

#### Scenario: Headless write in ask mode
- **WHEN** `harness ask --mode ask` needs to write a file
- **THEN** the write is denied, the model is informed, and the process exits with code 3

### Requirement: Shell commands run in an OS sandbox
Except in `full-access` mode, the system SHALL run `bash` commands and command-file shell expansions inside an OS sandbox (Seatbelt on macOS; Landlock and seccomp on Linux) that permits writes only to the workspace, temporary directories, and configured writable roots (none in `plan` and `read-only`), plus `/dev/shm` on Linux, and denies outbound network access. On Linux the sandbox MUST also refuse `connect`, `bind`, `listen`, and `accept` on Unix-domain sockets. When a command fails in a way that looks like a sandbox denial, the system MUST ask whether to re-run it without the sandbox, saying that the sandbox may have blocked it, since denials are recognised heuristically from the exit status and output; the re-run MUST NOT happen without approval. In `plan` and `read-only`, the system MUST report the denial instead and MUST NOT offer the re-run.

#### Scenario: Write outside the workspace
- **WHEN** a sandboxed command runs `touch ../outside.txt`
- **THEN** the command fails and `../outside.txt` does not exist

#### Scenario: Network blocked
- **WHEN** a sandboxed command runs `curl https://example.com` in `auto` mode
- **THEN** the connection fails and the user is offered an approval to re-run it without the sandbox

#### Scenario: Denial in plan mode
- **WHEN** a sandboxed command runs `curl https://example.com` in `plan` mode
- **THEN** the connection fails, the model is told the mode cannot run it without the read-only sandbox, and no re-run is offered

### Requirement: No silent unsandboxed fallback
When no sandbox mechanism is available, the system SHALL warn the user at startup, require approval for every `bash` command in `ask` and `auto`, and refuse `bash` commands in `plan` and `read-only`. A workspace that is the filesystem root, the user's home directory, or an ancestor of it MUST get no workspace-write sandbox, because it would make the user's dotfiles writable: in `ask` and `auto` the system MUST warn and treat it as having no sandbox, while `plan` and `read-only` keep their read-only sandbox. The system MUST NOT run a command unsandboxed without either `full-access` or an explicit approval.

#### Scenario: Linux kernel without Landlock
- **WHEN** harness starts on a Linux system where Landlock ABI 3 or later is not available, in `auto` mode
- **THEN** a warning is shown and each `bash` command asks for approval

#### Scenario: Workspace at the home directory
- **WHEN** harness runs in `auto` mode with the home directory as its workspace
- **THEN** a warning says the sandbox is off because the workspace is the home directory or above
- **AND** each `bash` command asks for approval

### Requirement: File tools enforce the workspace boundary
The system SHALL resolve `write` and `edit` target paths to canonical absolute paths, following symbolic links, before checking permissions. Targets outside the workspace MUST require approval in every mode except `full-access`, and MUST be rejected in `plan` and `read-only`.

#### Scenario: Symlink escape
- **WHEN** the model edits `link/file.txt` where `link` is a symlink to a directory outside the workspace, in `auto` mode
- **THEN** the edit requires approval

### Requirement: Shell commands are classified before they run
The system SHALL classify every shell command as allow-listed (every sub-command matches an allow rule), unlisted (fully decomposed with no deny, destructive, or confirm match), must-ask (undecomposable, destructive, or matching a `confirm` rule), or denied. In `auto`, `plan`, and `read-only` modes, allow-listed and unlisted commands MUST run in the sandbox without a prompt; in `ask` mode only allow-listed commands MAY run without a prompt; must-ask commands MUST prompt in every mode except `full-access`.

#### Scenario: Unlisted command in auto mode
- **WHEN** no rules are configured and the model runs `cargo build` in `auto` mode
- **THEN** the command runs in the sandbox without an approval prompt

#### Scenario: Unlisted command in ask mode
- **WHEN** no rules are configured and the model runs `cargo build` in `ask` mode
- **THEN** the user is asked to approve it

#### Scenario: Undecomposable command in auto mode
- **WHEN** the model runs `for f in *; do echo "$f"; done` in `auto` mode
- **THEN** the user is asked to approve it

### Requirement: The sandbox protects repository hooks and config
In workspace-write sandboxes the system SHALL protect `.git/config`, `.git/hooks`, `commondir`, the `.git` directory entry itself, a top-level `HEAD` file, and `.harness/` inside the workspace, while allowing other writes under `.git` so that commit, checkout, and stash work.

On macOS, and on Linux when unprivileged user namespaces are available (the full tier), writes to these paths MUST fail. On Linux, names that do not exist yet MUST be caught by a guard that moves them to a quarantine directory, never deleting them, and reports it in the tool result. The guard's scan for new repositories MUST use the ignore rules as they were when the session started, so that an ignore rule written during the session cannot hide a new repository.

When user namespaces are unavailable (the basic tier), the system MUST warn at startup and point to `harness sandbox doctor`. The guard MUST restore changed protected files after each command, and, while a process started by an earlier sandboxed command is still running, also before each later command. When harness exits, it MUST end the processes sandboxed commands left running and check once more. With `sandbox.linux_git_protection = "required"`, the basic tier MUST require approval for every shell command in `ask` and `auto`, including in a session that drops to the basic tier after it started: such a command then runs outside the sandbox only once approved, and a headless run MUST count it as blocked.

#### Scenario: Planting a hook
- **WHEN** a sandboxed command runs `echo x > .git/hooks/pre-commit` in `auto` mode on macOS, or on Linux in the full tier
- **THEN** the write fails

#### Scenario: Committing
- **WHEN** a sandboxed command runs `git commit --allow-empty -m test` in `auto` mode
- **THEN** the commit succeeds

#### Scenario: Planting a hook in the Linux basic tier
- **WHEN** user namespaces are blocked and a sandboxed command in `auto` mode writes `.git/hooks/pre-commit`
- **THEN** the file is moved to the quarantine directory after the command
- **AND** the tool result says so
- **AND** the command counts as blocked: headless runs exit 3 and no re-run outside the sandbox is offered

#### Scenario: A new nested repository on Linux
- **WHEN** a sandboxed command runs `git init sub` in `auto` mode on Linux
- **THEN** `sub/.git` is moved to the quarantine directory and the tool result says so

#### Scenario: Hiding a new repository behind an ignore rule
- **WHEN** a sandboxed command on Linux adds `sub/` to `.gitignore` and then runs `git init sub`
- **THEN** `sub/.git` is moved to the quarantine directory and the tool result says so

#### Scenario: A background process outlives harness
- **WHEN** a sandboxed command on Linux leaves a background process running and harness then exits
- **THEN** harness ends that process before it exits, undoes any change it made to protected files, and says so on stderr
- **AND** the exit code is unchanged

#### Scenario: A background process changes config in the Linux basic tier
- **WHEN** user namespaces are blocked and a sandboxed command starts a background process that rewrites `.git/config` after the command ends
- **THEN** `.git/config` is restored before the next command runs, the changed version is kept in the quarantine directory
- **AND** that command's result says so, without the command counting as blocked

#### Scenario: Strict git protection without user namespaces
- **WHEN** user namespaces are blocked, `sandbox.linux_git_protection = "required"`, and the model runs `ls` in `auto` mode
- **THEN** the user is asked to approve it

#### Scenario: Strict git protection after a drop to the basic tier
- **WHEN** a session in the full tier with `sandbox.linux_git_protection = "required"` drops to the basic tier and the model then runs `ls` in `auto` mode
- **THEN** the user is asked whether to run it outside the sandbox, and a headless run exits with code 3 without running it

### Requirement: Commands run in bash without startup files
The system SHALL run shell commands with `bash --noprofile --norc -c` with `BASH_ENV` and `ENV` removed from the environment. Bash MUST be looked for only at `/bin/bash`, `/usr/bin/bash`, and `/run/current-system/sw/bin/bash`, never on `PATH`, and `/bin/sh -c` MUST be used only when none of them exists.

#### Scenario: BASH_ENV is ignored
- **WHEN** `BASH_ENV` points to a script that creates a file and the model runs `true`
- **THEN** the file is not created
