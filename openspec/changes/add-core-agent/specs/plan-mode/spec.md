## Purpose

Plan mode lets the user explore a problem with the agent under read-only permissions and approve, edit, or refine a written plan before any code changes are made.

## ADDED Requirements

### Requirement: Plan mode is read-only exploration ending in a plan
In `plan` mode the system SHALL apply read-only permissions, instruct the model to investigate and finish with a step-by-step implementation plan, and present the plan with the choices Build, Edit, and Keep planning. The planning instruction MUST be appended to the conversation rather than changing the system prompt.

#### Scenario: Planning a feature
- **WHEN** the user enters `plan` mode and asks for a login rate limiter
- **THEN** the agent may read and search files but no file is modified
- **AND** the turn ends with a plan and the three choices

### Requirement: Approving a plan starts implementation
Choosing Build SHALL store the plan as the approved plan in the session, switch to the mode that was active before `plan` mode (or the default mode if there was none), and start a turn instructing the model to implement the approved plan.

#### Scenario: Build from plan
- **WHEN** the user was in `auto` mode, switched to `plan`, and chooses Build
- **THEN** the mode returns to `auto` and a new turn begins implementing the plan

### Requirement: Plans can be edited before approval
Choosing Edit SHALL open the plan in the user's `$EDITOR` (falling back to `vi`), and on save the edited text MUST become the plan presented for approval. Choosing Keep planning MUST keep `plan` mode and return to the input prompt.

#### Scenario: Editing the plan
- **WHEN** the user chooses Edit, deletes step 3, and saves
- **THEN** the plan shown for approval no longer contains step 3

### Requirement: Mode cycling
In interactive mode, Shift+Tab SHALL cycle the approval mode through `plan`, `ask`, and `auto`, and MUST never enter `full-access`. `/mode <name>` and `--mode <name>` MUST also select `plan`.

#### Scenario: Cycling modes
- **WHEN** the session is in `auto` mode and the user presses Shift+Tab
- **THEN** the mode becomes `plan` and the status line shows it
