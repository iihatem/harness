## Purpose

Checkpoints snapshot the workspace during a session so the user can rewind code, conversation, or both to an earlier point without touching their own git history.

## ADDED Requirements

### Requirement: Workspace snapshots before mutating actions
Before the first mutating action of each turn (`write`, `edit`, or `bash` in a mode that allows writes, including a slash command's shell part), the system SHALL snapshot the workspace into a shadow repository in the harness data directory. The snapshot MUST NOT modify the user's own git repository, index, branches, or history, and MUST work in directories that are not git repositories. Snapshots MUST honour `.gitignore`, including the repository's rules when the workspace is a subdirectory of a repository, exclude `.git`, `node_modules`, `target`, and `.harness/` and a `HEAD` at the top of the workspace, and skip files larger than 10 MB, whatever a `.gitignore` negation says. A workspace the repository's rules ignore MUST be snapshotted by its own ignore files, as a directory outside any repository. Each snapshot MUST record what existed but was left out, and the directory it was taken for. Files MUST be stored and restored byte for byte, whatever `.gitattributes` says.

#### Scenario: User's repository untouched
- **WHEN** a turn edits files in a git repository with staged changes
- **THEN** a checkpoint is created and the user's index, branches, and `git log` are unchanged

#### Scenario: Non-git directory
- **WHEN** a turn edits a file in a directory that is not a git repository
- **THEN** a checkpoint is created for that turn

#### Scenario: Files left out of snapshots
- **WHEN** the user rewinds code while the workspace holds a 50 MB file or a git-ignored file, also where the checkpoint has a directory of the same name
- **THEN** that file is neither overwritten nor deleted

#### Scenario: File left out when the checkpoint was taken
- **WHEN** a file was git-ignored, over 10 MB or unreadable when the checkpoint was taken, and has since been un-ignored, shrunk or made readable, and the user rewinds code to that checkpoint
- **THEN** that file is not deleted

#### Scenario: Workspace in a repository's subdirectory
- **WHEN** harness runs in a subdirectory of a repository whose root `.gitignore` ignores `.env` and `*.log`
- **THEN** snapshots leave that subdirectory's `.env` and log files out, and rewinding code neither reverts nor deletes them

#### Scenario: Workspace its repository ignores
- **WHEN** harness runs in a directory its repository's `.gitignore` ignores, and the agent changes files there
- **THEN** snapshots hold that directory's files, and rewinding code restores them

#### Scenario: Rewind empties a subdirectory workspace
- **WHEN** harness runs in an empty subdirectory of a repository, the agent creates files there, and the user rewinds code to before that
- **THEN** the files are removed and the subdirectory, and the directories above it, still exist

### Requirement: Rewind restores code, conversation, or both
The system SHALL provide `/rewind` (and Esc pressed twice on empty input) listing the session's previous user messages. After the user selects one, the system MUST offer to restore code and conversation, code only, or conversation only, to the state before that message. Restoring code MUST revert modified files, recreate deleted files, and remove files created since that point, including changes made by `bash` and by a slash command's shell parts. A checkpoint MUST be restored only in the directory it was taken for, and under the same work tree (a change in the repository's ignore rules for that directory can change it), and code MUST NOT be restored across a turn that changed files while checkpoints were off. A restore MUST NOT write through a symlink, MUST NOT remove the workspace directory, and MUST give files only their owner could read their permissions again.

#### Scenario: Undo a bad refactor
- **WHEN** the agent modified three files and ran a formatter via `bash`, and the user rewinds code and conversation to before that turn
- **THEN** all four kinds of change are reverted and the conversation continues from before that user message

#### Scenario: Conversation only
- **WHEN** the user rewinds conversation only
- **THEN** workspace files are unchanged and the next turn continues from the selected point

#### Scenario: Slash command with a shell part
- **WHEN** a slash command's `!` shell part changed a file and the user rewinds code to before that command
- **THEN** the file is restored

#### Scenario: Session continued from another directory
- **WHEN** a session is continued from a subdirectory of the directory its checkpoints were taken in, and the user rewinds code
- **THEN** the rewind is refused and no file changes; rewinding the conversation still works

#### Scenario: Ignore rules changed since the checkpoint
- **WHEN** the repository ignored the workspace when a checkpoint was taken and no longer does (or the reverse), and the user continues the session and rewinds code
- **THEN** the rewind is refused and no file changes

#### Scenario: Turn without a checkpoint
- **WHEN** a turn changed files while checkpoints were off, and the user rewinds code to before that turn
- **THEN** the rewind is refused and no file changes

### Requirement: Rewinds are themselves reversible
Before restoring code, for a rewind or for undoing one, the system SHALL snapshot the current workspace and keep that snapshot in the session. After a rewind, the rewind list MUST offer an "undo last rewind" entry that restores the workspace and the active conversation branch to their state immediately before that rewind. A rewind whose restore fails partway MUST be recorded so that it can be undone the same way. Rewinding the conversation MUST create a new branch in the session rather than deleting entries.

#### Scenario: Rewind by mistake
- **WHEN** the user rewinds code and conversation to an earlier turn and then chooses "undo last rewind"
- **THEN** the workspace files and the conversation return to their state before the first rewind

#### Scenario: Restore that fails partway
- **WHEN** restoring code fails after some files were restored, for example because a directory is read-only
- **THEN** the rewind reports the failure, the conversation is unchanged, and "undo last rewind" returns the files to their state before it

### Requirement: Rewind limits are stated and failures degrade safely
The rewind interface SHALL state that effects outside the workspace (network calls, databases, pushed commits, files outside the workspace) and what is inside nested git repositories and submodules are not reverted. When the `git` executable is unavailable, a snapshot fails or takes longer than 5 seconds, or the shadow repository is inside the workspace or a directory sandboxed commands can write to, the system MUST disable checkpoints for the session with a warning instead of blocking the turn.

#### Scenario: git missing
- **WHEN** harness starts on a machine without `git` on `PATH`
- **THEN** a warning states that checkpoints are disabled and turns proceed normally

#### Scenario: Data directory where commands can write
- **WHEN** harness's data directory is inside the workspace, or inside a temporary or configured writable directory while the OS sandbox is on
- **THEN** a warning states that checkpoints are disabled and names the directory, and turns proceed normally
