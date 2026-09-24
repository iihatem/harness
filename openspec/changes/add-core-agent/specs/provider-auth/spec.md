## Purpose

Provider authentication manages how the harness obtains, stores, refreshes, and protects credentials for model providers, including multiple accounts per provider, within each vendor's terms of use.

## ADDED Requirements

### Requirement: API keys come from the environment or the credential store
The system SHALL resolve a provider's API key from its configured environment variable first and from the credential store for the active account profile second. `harness auth add <provider>` MUST store a key in the OS keychain. When no keychain service is available, the key MUST be stored in `credentials.json` in the harness data directory with file mode 0600, never in the configuration directory, and the user MUST be warned.

#### Scenario: Environment variable wins
- **WHEN** both `OPENAI_API_KEY` and a stored key for `openai` exist
- **THEN** requests to `openai` use the environment variable's value

#### Scenario: No keychain on a headless Linux host
- **WHEN** the user runs `harness auth add openrouter` and no keychain service is available
- **THEN** the key is written to `credentials.json` in the data directory with mode 0600 and a warning is shown

### Requirement: Multiple account profiles per provider
The system SHALL store credentials per provider and named account profile, with `default` as the unnamed profile. `harness login <provider> --profile <name>` and `harness auth add <provider> --profile <name>` MUST store credentials under that profile, and `harness auth use <provider> <name>` MUST make that profile the active one for the provider.

#### Scenario: Switching ChatGPT accounts
- **WHEN** the user has signed in with profiles `personal` and `work` and runs `harness auth use chatgpt work`
- **THEN** subsequent `chatgpt/*` requests use the `work` account's credentials

### Requirement: ChatGPT sign-in
The system SHALL provide `harness login chatgpt`, which signs the user in through a browser-based OAuth flow with PKCE and a localhost callback, and SHALL offer a device-code flow when `--device` is given or a browser cannot be opened. Tokens MUST be stored in the credential store and refreshed automatically before expiry or after a single 401 response. The login flow MUST tell the user that ChatGPT subscription use in third-party tools relies on OpenAI's current practice rather than a contractual guarantee.

#### Scenario: Sign-in over SSH
- **WHEN** the user runs `harness login chatgpt` in an SSH session without a browser
- **THEN** the system displays a device code and verification URL and completes sign-in once the user approves

#### Scenario: Expired access token
- **WHEN** a request with the stored ChatGPT token returns 401 and the refresh token is valid
- **THEN** the system refreshes the token, retries the request once, and the turn continues

### Requirement: Credentials can be removed
The system SHALL provide `harness logout <provider> [--profile <name>]`, which removes that provider's stored credentials for the given profile (default: the active profile) from the credential store.

#### Scenario: Logout
- **WHEN** the user runs `harness logout chatgpt` with only the default profile signed in
- **THEN** subsequent `chatgpt/*` requests report that the user is not signed in

### Requirement: Claude subscription credentials are never used
The system MUST NOT read, store, request, or use Claude.ai subscription credentials or session tokens, including those belonging to an installed Claude Code. The `anthropic` provider MUST authenticate only with an Anthropic API key.

#### Scenario: Claude Code signed in, no API key
- **WHEN** Claude Code is installed and signed in, and no Anthropic API key is configured
- **THEN** selecting an `anthropic/*` model reports that an API key is required
- **AND** no file under `~/.claude` is read for credentials

### Requirement: Secrets are redacted everywhere
The system MUST NOT write API keys, OAuth access tokens, or refresh tokens to logs, session files, tool-output files, NDJSON output, or error messages.

#### Scenario: Debug logging
- **WHEN** a turn runs with `--debug` using an API key provider
- **THEN** neither the log file nor the session file contains the key's value
