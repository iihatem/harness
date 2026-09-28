## Purpose

Project context gives the model the user's standing instructions and basic facts about the environment, drawn from the same instruction files other coding agents use, assembled into a prompt prefix that stays stable so providers can reuse their caches.

## ADDED Requirements

### Requirement: Instruction files are discovered hierarchically
The system SHALL load the global `AGENTS.md` from the harness configuration directory and, in each directory from the discovery root down to the working directory, that directory's `AGENTS.md`, or its `CLAUDE.md` when no `AGENTS.md` exists. The discovery root is the repository root; outside a repository it is the user's home directory when the working directory is inside it, and otherwise the working directory itself. Files MUST be ordered from global to most specific.

#### Scenario: AGENTS.md preferred
- **WHEN** a directory contains both `AGENTS.md` and `CLAUDE.md`
- **THEN** only `AGENTS.md` from that directory is loaded

#### Scenario: CLAUDE.md fallback
- **WHEN** a directory contains only `CLAUDE.md`
- **THEN** `CLAUDE.md` from that directory is loaded

### Requirement: File imports are resolved safely
The system SHALL expand `@<path>` import lines (lines holding only the import, outside code fences) in instruction files relative to the importing file, to a maximum depth of 5. Imports MUST be confined to the discovery root and the harness configuration directory; outside a repository they MUST be confined to the importing file's own directory and the harness configuration directory. Each file MUST be included at most once, and a missing or disallowed import MUST produce a warning rather than a failure. An instruction file found in the project that resolves, through symlinks, outside the repository and the harness configuration directory MUST be skipped with a warning; outside a repository, one that resolves outside the directory it was found in and the harness configuration directory MUST be skipped with a warning.

#### Scenario: CLAUDE.md importing AGENTS.md
- **WHEN** a subdirectory has only a `CLAUDE.md` containing `@../AGENTS.md`, and the parent's `AGENTS.md` is already loaded
- **THEN** the parent `AGENTS.md` content appears once in the context

#### Scenario: Import outside the repository
- **WHEN** an instruction file contains `@/etc/passwd`
- **THEN** the import is skipped with a warning

#### Scenario: Instruction file linked to a secret
- **WHEN** a repository's `AGENTS.md` is a symlink to `~/.aws/credentials`
- **THEN** it is skipped with a warning and its target is not sent to the model

#### Scenario: Instruction file outside a repository linked to a secret
- **WHEN** a folder in the home directory that is not a repository, such as an extracted archive, has an `AGENTS.md` that is a symlink to `~/.aws/credentials`
- **THEN** it is skipped with a warning and its target is not sent to the model, while a symlink to a file inside that folder is loaded

### Requirement: Environment information is captured at session start
The system SHALL include the working directory, operating system, date, and, inside a git repository, the current branch and whether the work tree has uncommitted changes, all captured once when the session starts. Capturing them MUST NOT run any program that the repository's configuration, or a submodule's, names (a file-system monitor, a hook, or a filter driver); when a filter driver is configured, whether there are uncommitted changes MUST be left out.

#### Scenario: Dirty git repository
- **WHEN** the session starts in a repository with uncommitted changes on branch `main`
- **THEN** the context states the branch `main` and that there are uncommitted changes

#### Scenario: Repository whose configuration names a program
- **WHEN** the session starts in a repository whose `.git/config` sets `core.fsmonitor` to a script, or defines a clean filter that its `.gitattributes` assigns to a modified file
- **THEN** the script and the filter never run, the context still states the branch, and with the filter configured it does not say whether there are uncommitted changes

### Requirement: The prompt prefix is stable within a session
The system SHALL keep the system prompt and tool definitions byte-identical across all requests in a session, rebuilding them only after compaction or a model switch. Mode changes, planning instructions, and other mid-session context MUST be appended as messages instead of modifying the system prompt. The base system prompt, excluding instruction files and environment information, MUST NOT exceed 1,000 tokens.

#### Scenario: Mode change mid-session
- **WHEN** the user switches from `auto` to `plan` between two turns
- **THEN** the system prompt in both requests is byte-identical and the mode change appears as an appended message

#### Scenario: Date changes during a long session
- **WHEN** a session started before midnight continues after midnight
- **THEN** the system prompt still contains the session's start date

### Requirement: Oversized instructions are flagged
The system SHALL warn the user when the loaded instruction files exceed 25% of the active model's effective context window.

#### Scenario: Large AGENTS.md with a small local model
- **WHEN** instruction files total 3,000 tokens and the effective context window is 8,192 tokens
- **THEN** the user sees a warning naming the files and their size

### Requirement: /init drafts an AGENTS.md
The system SHALL provide `/init`, which asks the model to inspect the repository and draft an `AGENTS.md`. It MUST NOT overwrite an existing `AGENTS.md` without the user's confirmation.

#### Scenario: Existing AGENTS.md
- **WHEN** the user runs `/init` in a repository that already has `AGENTS.md`
- **THEN** the proposed content is shown and the file is changed only after confirmation
