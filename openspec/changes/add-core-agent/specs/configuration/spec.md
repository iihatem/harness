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
The system SHALL apply project-level settings that widen what the agent may do (a `mode` other than `plan`, `read-only`, or `ask`; `model`; `allow` rules; `read_dirs`; provider definitions or `base_url` overrides; `[sandbox]` settings) only when the user has trusted the workspace with the current set of those settings. Trust MUST be recorded in the data directory as a fingerprint of the widening settings; when they change, the workspace MUST be treated as untrusted until trusted again. On first interactive use of a workspace with such settings, the system MUST display them and ask whether to trust the workspace. Headless runs MUST ignore untrusted widening settings with a warning. Narrowing settings (`deny`, `confirm`, stricter modes) MUST always apply.

#### Scenario: Cloned repository redirects a provider
- **WHEN** a cloned repository's `.harness/config.toml` sets `providers.openai.base_url` to an unknown host and the workspace is not trusted
- **THEN** harness shows the setting and asks for trust before using it
- **AND** until trusted, requests to `openai` go to the default endpoint

#### Scenario: Deny rules apply without trust
- **WHEN** an untrusted project config contains `deny = ["bash:git push*"]`
- **THEN** `git push` is blocked in that project

#### Scenario: Settings change after trust
- **WHEN** a trusted workspace's project config later gains `providers.x.base_url`
- **THEN** the widening settings are ignored with a warning until the workspace is trusted again

### Requirement: Trusting a workspace from the command line
The system SHALL provide `harness trust`, which displays the workspace's widening project settings and records trust after the user confirms interactively or passes `--yes`, and `harness trust --revoke`, which removes it. Without a terminal and without `--yes`, `harness trust` MUST exit with code 2 and explain how to confirm.

#### Scenario: Trusting non-interactively
- **WHEN** the user runs `harness trust --yes` in a workspace whose project config has an `allow` rule
- **THEN** later runs in that workspace apply the rule without a warning
