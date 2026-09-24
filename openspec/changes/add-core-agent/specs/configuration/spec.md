## Purpose

Configuration defines where harness keeps its settings and data, how global, project, and command-line settings combine, and how project-level settings are prevented from silently widening what the agent can do.

## ADDED Requirements

### Requirement: XDG base directories
The system SHALL store configuration in `$XDG_CONFIG_HOME/harness` (default `~/.config/harness`), data such as sessions, checkpoints, the trust list, and the credential fallback file in `$XDG_DATA_HOME/harness` (default `~/.local/share/harness`), and logs and tool output in `$XDG_STATE_HOME/harness` (default `~/.local/state/harness`), on both macOS and Linux. When `HARNESS_HOME` is set, the system MUST use `config`, `data`, and `state` subdirectories of that path instead.

#### Scenario: Custom XDG_CONFIG_HOME
- **WHEN** `XDG_CONFIG_HOME=/tmp/cfg` is set and the user saves a default model
- **THEN** the setting is written to `/tmp/cfg/harness/config.toml`

#### Scenario: HARNESS_HOME override
- **WHEN** `HARNESS_HOME=/opt/h` is set
- **THEN** sessions are stored under `/opt/h/data/sessions/`

### Requirement: Layered configuration
The system SHALL read TOML configuration from the global `config.toml`, then the project's `.harness/config.toml`, then command-line flags, with later layers overriding earlier ones for scalar settings and rule lists being combined. Invalid configuration MUST produce an error naming the file, line, and problem, and MUST NOT be silently ignored.

#### Scenario: Project overrides the default model
- **WHEN** the global config sets `model = "ollama/a"` and the trusted project config sets `model = "ollama/b"`
- **THEN** sessions in that project use `ollama/b`

#### Scenario: Typo in config
- **WHEN** the global config contains `mdoe = "auto"`
- **THEN** harness reports the unknown key with its file and line

### Requirement: Widening project settings require workspace trust
The system SHALL apply project-level settings that widen what the agent may do (a `mode` other than `plan`, `read-only`, or `ask`; `allow` rules; `read_dirs`; provider definitions or `base_url` overrides) only when the user has trusted the workspace. On first interactive use of a workspace with such settings, the system MUST display them and ask whether to trust the workspace, and MUST remember the decision in the data directory. Headless runs MUST ignore untrusted widening settings with a warning. Narrowing settings (`deny`, `confirm`, stricter modes) MUST always apply.

#### Scenario: Cloned repository redirects a provider
- **WHEN** a cloned repository's `.harness/config.toml` sets `providers.openai.base_url` to an unknown host and the workspace is not trusted
- **THEN** harness shows the setting and asks for trust before using it
- **AND** until trusted, requests to `openai` go to the default endpoint

#### Scenario: Deny rules apply without trust
- **WHEN** an untrusted project config contains `deny = ["bash:git push*"]`
- **THEN** `git push` is blocked in that project
