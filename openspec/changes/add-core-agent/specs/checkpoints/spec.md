## Purpose

Checkpoints snapshot the workspace during a session so the user can rewind code, conversation, or both to an earlier point without touching their own git history.

## ADDED Requirements

### Requirement: Workspace snapshots before mutating actions
Before the first mutating action of each turn (`write`, `edit`, or `bash` in a mode that allows writes), the system SHALL snapshot the workspace into a shadow repository in the harness data directory. The snapshot MUST NOT modify the user's own git repository, index, branches, or history, and MUST work in directories that are not git repositories. Snapshots MUST honour `.gitignore`, exclude `.git`, `node_modules`, and `target`, and skip files larger than 10 MB.

#### Scenario: User's repository untouched
- **WHEN** a turn edits files in a git repository with staged changes
- **THEN** a checkpoint is created and the user's index, branches, and `git log` are unchanged

#### Scenario: Non-git directory
- **WHEN** a turn edits a file in a directory that is not a git repository
- **THEN** a checkpoint is created for that turn

### Requirement: Rewind restores code, conversation, or both
The system SHALL provide `/rewind` (and Esc pressed twice on empty input) listing the session's previous user messages. After the user selects one, the system MUST offer to restore code and conversation, code only, or conversation only, to the state before that message. Restoring code MUST revert modified files, recreate deleted files, and remove files created since that point, including changes made by `bash`.

#### Scenario: Undo a bad refactor
- **WHEN** the agent modified three files and ran a formatter via `bash`, and the user rewinds code and conversation to before that turn
- **THEN** all four kinds of change are reverted and the conversation continues from before that user message

#### Scenario: Conversation only
- **WHEN** the user rewinds conversation only
- **THEN** workspace files are unchanged and the next turn continues from the selected point

### Requirement: Rewinds are themselves reversible
Before restoring code, the system SHALL snapshot the current workspace. After a rewind, the rewind list MUST offer an "undo last rewind" entry that restores the workspace and the active conversation branch to their state immediately before that rewind. Rewinding the conversation MUST create a new branch in the session rather than deleting entries.

#### Scenario: Rewind by mistake
- **WHEN** the user rewinds code and conversation to an earlier turn and then chooses "undo last rewind"
- **THEN** the workspace files and the conversation return to their state before the first rewind

### Requirement: Rewind limits are stated and failures degrade safely
The rewind interface SHALL state that effects outside the workspace (network calls, databases, pushed commits, files outside the workspace) are not reverted. When the `git` executable is unavailable, or a snapshot takes longer than 5 seconds, the system MUST disable checkpoints for the session with a warning instead of blocking the turn.

#### Scenario: git missing
- **WHEN** harness starts on a machine without `git` on `PATH`
- **THEN** a warning states that checkpoints are disabled and turns proceed normally
