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
The system SHALL apply project-level settings that widen what the agent may do (a `mode` that grants more than the effective global mode; a `max_steps` above the effective global limit; `model`; `allow` rules; `read_dirs`; provider definitions or `base_url` overrides; model `[profiles]`; `[sandbox]` settings other than `allow_localhost = false` and `linux_git_protection = "required"`) only when the user has trusted the workspace with the current set of those settings. The effective global mode is the global config's `mode`, or else the default mode for the workspace, with modes ranked `plan` = `read-only` < `ask` < `auto` < `full-access`; the effective global limit is the global `max_steps`, or else the built-in default. Trust MUST be recorded in the data directory as a fingerprint of the widening settings; when they change, the workspace MUST be treated as untrusted until trusted again. On first interactive use of a workspace with such settings, the system MUST display them and ask whether to trust the workspace; trusting MUST record trust as `harness trust` does, so the settings apply to that session, and declining MUST leave them unapplied and ask again at the next interactive start. Headless runs MUST ignore untrusted widening settings with a warning. Narrowing settings (`deny`, `confirm`, a `mode` or `max_steps` no wider than the effective global one, `allow_localhost = false`, and `linux_git_protection = "required"`) MUST always apply.

#### Scenario: Cloned repository redirects a provider
- **WHEN** a cloned repository's `.harness/config.toml` sets `providers.openai.base_url` to an unknown host and the workspace is not trusted
- **THEN** harness shows the setting and asks for trust before using it
- **AND** until trusted, requests to `openai` go to the default endpoint

#### Scenario: Trusting on first interactive use
- **WHEN** the user starts `harness --mode ask` in a cloned repository whose project config has `allow = ["bash:make *"]`, and answers yes
- **THEN** `make test` runs without an approval prompt in that session, and later sessions do not ask about trust again

#### Scenario: Deny rules apply without trust
- **WHEN** an untrusted project config contains `deny = ["bash:git push*"]`
- **THEN** `git push` is blocked in that project

#### Scenario: Project mode compared with the global mode
- **WHEN** the global config sets `mode = "plan"` and an untrusted project config sets `mode = "ask"`
- **THEN** the session stays in `plan` mode and a warning names the ignored `mode` setting
- **AND** with a global `mode = "full-access"`, a project `mode = "auto"` applies without trust

#### Scenario: Settings change after trust
- **WHEN** a trusted workspace's project config later gains `providers.x.base_url`
- **THEN** the widening settings are ignored with a warning until the workspace is trusted again

### Requirement: Compaction settings
The system SHALL read `[compaction] threshold_percent` (default 80) and `keep_recent_percent` (default 20) as whole percentages of the context window. Each MUST be between 1 and 100, and the kept share below the threshold; other values MUST be errors naming the file. A project's `threshold_percent` below 50 MUST apply only in a trusted workspace, and is shown and fingerprinted by `harness trust` with the widening settings; otherwise the global value, or the default, applies, with a warning naming the project file. The global config, and a trusted workspace's project config, MAY set any valid value; the project's other compaction settings apply without trust. Project settings MUST be checked as they would apply once trusted, so a project config that is invalid then is an error while untrusted too; while its low threshold is ignored, its `keep_recent_percent` applies only when it is below the threshold that applies instead.

#### Scenario: A project asks to compact early
- **WHEN** an untrusted project config sets `threshold_percent = 30` and the global config sets no threshold
- **THEN** compaction starts at 80% of the context window, and a warning names `.harness/config.toml` and the ignored setting

#### Scenario: Keeping nothing
- **WHEN** a config sets `keep_recent_percent = 0`
- **THEN** harness reports the invalid value with its file

### Requirement: Trusting a workspace from the command line
The system SHALL provide `harness trust`, which displays the workspace's widening project settings and records trust after the user confirms interactively or passes `--yes`, and `harness trust --revoke`, which removes it. A workspace with no widening settings MUST be trustable too: its trust is recorded as the fingerprint of the empty set, so it lasts until a widening setting appears. Without a terminal and without `--yes`, `harness trust` MUST exit with code 2 and explain how to confirm.

#### Scenario: Trusting non-interactively
- **WHEN** the user runs `harness trust --yes` in a workspace whose project config has an `allow` rule
- **THEN** later runs in that workspace apply the rule without a warning

#### Scenario: Trusting a workspace that has only command files
- **WHEN** a repository has `.claude/commands/pick.md` with `model: other/model`, no project config, and the user runs `harness trust --yes`
- **THEN** later runs of `/pick` use `other/model`
- **AND** once the project config gains an `allow` rule, the workspace is untrusted until trusted again
