## Purpose

Built-in tools give the model a small, predictable set of actions for reading, searching, modifying files, and running commands in the workspace.

## ADDED Requirements

### Requirement: read returns numbered file content
The `read` tool SHALL return a text file's content with line numbers, accept an optional starting line and line limit, and truncate large files with a notice stating how to read further. It MUST refuse binary files with an error result.

#### Scenario: Reading part of a file
- **WHEN** the model reads `src/main.rs` with offset 100 and limit 50
- **THEN** it receives lines 100 through 149 with their line numbers

#### Scenario: Binary file
- **WHEN** the model reads a PNG file
- **THEN** it receives an error result stating the file is binary

### Requirement: write creates files and guards overwrites
The `write` tool SHALL create a new file with the given content, creating parent directories as needed. Overwriting an existing file MUST fail unless the file was read with `read` in the current session and has not changed on disk since that read.

#### Scenario: Overwriting an unread file
- **WHEN** the model writes to an existing file it has not read in this session
- **THEN** the write fails with an error instructing it to read the file first

#### Scenario: File changed externally
- **WHEN** the model read a file, the user then modified it in an editor, and the model writes to it
- **THEN** the write fails with an error stating the file changed since it was read

### Requirement: edit performs exact, unambiguous replacements
The `edit` tool SHALL replace an exact text match in a file and return a unified diff of the change. The match MUST occur exactly once unless `replace_all` is set; zero or multiple matches MUST fail with an error stating the match count. The same read-before-modify rule as `write` MUST apply.

#### Scenario: Ambiguous match
- **WHEN** the target text occurs 3 times and `replace_all` is not set
- **THEN** the edit fails with an error reporting 3 matches and the file is unchanged

### Requirement: bash runs bounded, non-interactive commands
The `bash` tool SHALL run a non-interactive shell command in the workspace directory and return its exit code and combined output. Commands MUST time out after 120 seconds by default (configurable per call up to 600 seconds), with the whole process group terminated on timeout.

#### Scenario: Timeout
- **WHEN** a command runs longer than its timeout
- **THEN** the process group is terminated and the model receives an error result stating the timeout

### Requirement: Large tool output is saved to a file
When a tool's output exceeds 10 KB or the output budget derived from the active model's context window, the system SHALL save the full output to a file in the harness tool-output directory and return to the model the beginning and end of the output within the budget, the omitted size, and the file's path. The model MUST be able to `read` that file without an approval prompt.

#### Scenario: Very long test output
- **WHEN** a command prints 50,000 lines
- **THEN** the model receives the first and last lines within the budget, the number of omitted lines, and the path of a file containing the full output
- **AND** reading that path does not trigger an approval prompt

### Requirement: grep and glob respect ignore files and cap results
The `grep` tool SHALL search file contents by regular expression and the `glob` tool SHALL match file paths by pattern. Both MUST honour `.gitignore` and related ignore files, and MUST cap results with a notice when the cap is reached.

#### Scenario: Ignored directories
- **WHEN** the model greps for a symbol in a project with `target/` listed in `.gitignore`
- **THEN** no matches from `target/` are returned

### Requirement: Tool definitions are stable and compact
The system SHALL send tool definitions in a fixed order with identical text on every request within a session, and the six built-in tool definitions together MUST NOT exceed 1,500 tokens.

#### Scenario: Consecutive requests
- **WHEN** two consecutive model requests are made in the same session
- **THEN** the serialized tool definitions in both requests are byte-identical
