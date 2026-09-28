## Purpose

Sessions preserve conversations on disk so they survive crashes, can be resumed and branched later, and stay within the model's context window through compaction.

## ADDED Requirements

### Requirement: Sessions are persisted incrementally
The system SHALL persist each session as an append-only JSON Lines file in the `sessions/<project-key>/` directory of the harness data directory, where `<project-key>` is derived from the canonical repository root path, or from the canonical working directory outside a repository. Each entry MUST be appended as soon as it is complete and MUST carry an `id` and a `parent_id`.

#### Scenario: Entries survive a crash
- **WHEN** the harness process is killed after two completed turns
- **THEN** the session file contains both turns

#### Scenario: Session in use
- **WHEN** a second harness process tries to continue a session that another process is using
- **THEN** it refuses with an error instead of writing to the file

### Requirement: Sessions branch instead of losing history
The system SHALL treat the session as a tree of entries whose active branch runs from the root to the current leaf. Rewinding the conversation MUST move the current leaf to an earlier entry, and new entries MUST be appended as children of that entry, leaving the previous branch intact in the file.

#### Scenario: Branch after rewind
- **WHEN** the user rewinds to before their third message and sends a different message
- **THEN** the session file still contains the original third message and its replies
- **AND** the model only sees the new branch

### Requirement: Sessions can be resumed
The system SHALL resume the most recent session for the current project with `harness -c`, and SHALL let the user choose a session with `harness --resume` or `/resume`, showing each session's start time and first user message. Resuming MUST load the session's active branch. `/new` MUST start a new session.

#### Scenario: Continue latest
- **WHEN** the user runs `harness -c` in a project with previous sessions
- **THEN** the most recent session's active branch is loaded and the next turn has access to it

### Requirement: Truncated session files are tolerated
The system SHALL ignore an incomplete final line when loading a session file and warn the user, loading all complete entries.

#### Scenario: Half-written last line
- **WHEN** a session file ends with a partial JSON line
- **THEN** the session loads with all complete entries and a warning is shown

### Requirement: Context is compacted automatically and on demand
The system SHALL compact the conversation when estimated context usage reaches a configurable threshold (default 80% of the effective context window) and when the user runs `/compact`, optionally with focus instructions. Compaction MUST replace older history with a model-generated summary while keeping the most recent turns verbatim within a configurable budget (default 20% of the context window). The summary MUST be shown to the user and stored as a compaction entry, and the original entries MUST remain in the file so the user can rewind to before the compaction.

#### Scenario: Automatic compaction
- **WHEN** a turn would bring estimated usage above 80% of the context window
- **THEN** a compaction event is emitted before the next model call, the summary is displayed, and the request fits within the window

### Requirement: Context overflow triggers compaction and one retry
When a provider rejects a request because it exceeds the context window, the system SHALL compact the conversation and retry the request once. If the retry also fails, the system MUST emit an error event.

#### Scenario: Provider reports context overflow
- **WHEN** the provider responds with a context-length error
- **THEN** the conversation is compacted and the request is retried once
