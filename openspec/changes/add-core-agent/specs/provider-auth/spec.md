## Purpose

Provider authentication manages how the harness obtains, stores, refreshes, and protects credentials for model providers, including multiple accounts per provider, within each vendor's terms of use.

## ADDED Requirements

### Requirement: API keys come from the environment or the credential store
The system SHALL resolve a provider's API key from its configured environment variable first and from the credential store for the active account profile second. `harness auth add <provider>` MUST read the key from standard input, without echoing it on a terminal, and MUST store it in the OS keychain. When no keychain service is available, the key MUST be stored in `credentials.json` in the harness data directory with file mode 0600, never in the configuration directory, and the user MUST be warned.

#### Scenario: Environment variable wins
- **WHEN** both `OPENAI_API_KEY` and a stored key for `openai` exist
- **THEN** requests to `openai` use the environment variable's value

#### Scenario: No keychain on a headless Linux host
- **WHEN** the user runs `harness auth add openrouter` and no keychain service is available
- **THEN** the key is written to `credentials.json` in the data directory with mode 0600 and a warning is shown

#### Scenario: Key piped from a password manager
- **WHEN** the user runs `pass show openai | harness auth add openai`
- **THEN** the key is stored without its trailing newline, and it never appears in the command line or the shell history

### Requirement: Multiple account profiles per provider
The system SHALL store credentials per provider and named account profile, with `default` as the unnamed profile. `harness login <provider> --profile <name>` and `harness auth add <provider> --profile <name>` MUST store credentials under that profile, and `harness auth use <provider> <name>` MUST make that profile the active one for the provider.

#### Scenario: Switching ChatGPT accounts
- **WHEN** the user has signed in with profiles `personal` and `work` and runs `harness auth use chatgpt work`
- **THEN** subsequent `chatgpt/*` requests use the `work` account's credentials

### Requirement: ChatGPT sign-in
The system SHALL provide `harness login chatgpt`, which signs the user in through a browser-based OAuth flow with PKCE and a localhost callback, and SHALL offer a device-code flow when `--device` is given or a browser cannot be opened. Tokens MUST be stored in the credential store and refreshed automatically before expiry or after a single 401 response. Before refreshing, the system MUST read the stored tokens again and use them when another process has already refreshed them. The login flow MUST tell the user that ChatGPT subscription use in third-party tools relies on OpenAI's current practice rather than a contractual guarantee.

#### Scenario: Sign-in over SSH
- **WHEN** the user runs `harness login chatgpt` in an SSH session without a browser
- **THEN** the system displays a device code and verification URL and completes sign-in once the user approves

#### Scenario: Expired access token
- **WHEN** a request with the stored ChatGPT token returns 401 and the refresh token is valid
- **THEN** the system refreshes the token, retries the request once, and the turn continues

#### Scenario: Two sessions refresh at once
- **WHEN** another harness process refreshed the stored ChatGPT tokens after this process read them, and this process's request returns 401
- **THEN** this process uses the stored tokens without asking the authorization server again

### Requirement: Credentials can be removed
The system SHALL provide `harness logout <provider> [--profile <name>]`, which removes that provider's stored credentials for the given profile (default: the active profile) from the credential store.

#### Scenario: Logout
- **WHEN** the user runs `harness logout chatgpt` with only the default profile signed in
- **THEN** subsequent `chatgpt/*` requests report that the user is not signed in

### Requirement: Claude subscription credentials are never used
The system MUST NOT read, store, request, or use Claude.ai subscription credentials or session tokens, including those belonging to an installed Claude Code. The `anthropic` provider MUST authenticate only with an Anthropic API key. A Claude subscription token (`sk-ant-oat…`) MUST be refused wherever it is given, and `harness login anthropic` MUST explain that an API key is required.

#### Scenario: Claude Code signed in, no API key
- **WHEN** Claude Code is installed and signed in, and no Anthropic API key is configured
- **THEN** selecting an `anthropic/*` model reports that an API key is required
- **AND** no file under `~/.claude` is read for credentials

#### Scenario: A subscription token in the environment
- **WHEN** `ANTHROPIC_API_KEY` holds a Claude subscription token and the user selects an `anthropic/*` model
- **THEN** harness refuses it with an explanation and sends no request

### Requirement: Secrets are redacted everywhere
The system MUST NOT write API keys, OAuth access tokens, or refresh tokens to logs, session files, tool-output files, NDJSON output, error messages, or anything else it prints. The values of environment variables whose names end in `KEY`, `TOKEN`, `SECRET` or `PASSWORD` MUST be treated as secrets too.

#### Scenario: Debug logging
- **WHEN** a turn runs with `--debug` using an API key provider
- **THEN** neither the log file nor the session file contains the key's value

#### Scenario: A command prints the environment
- **WHEN** the model runs `printenv` through the `bash` tool while `OPENAI_API_KEY` is set
- **THEN** the session file, the tool-output files and the NDJSON output show `[redacted]` in place of the key's value
