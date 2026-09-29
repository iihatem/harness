## Purpose

Provider authentication manages how the harness obtains, stores, refreshes, and protects credentials for model providers, including multiple accounts per provider, within each vendor's terms of use.

## ADDED Requirements

### Requirement: API keys come from the environment or the credential store
The system SHALL resolve a provider's API key from its configured environment variable first and from the credential store for the active account profile second. `harness auth add <provider>` MUST read the key from standard input, without echoing it on a terminal, and MUST store it in the OS keychain. When no keychain service is available, the key MUST be stored in `credentials.json` in the harness data directory with file mode 0600, never in the configuration directory, and the user MUST be warned. A keychain that refuses a new key MUST NOT leave an older key it holds in use: the older key is removed, or storing fails with an error that says so. A credential store that cannot be read MUST be reported as such, naming the file and how to recover, never taken for an empty one.

#### Scenario: Environment variable wins
- **WHEN** both `OPENAI_API_KEY` and a stored key for `openai` exist
- **THEN** requests to `openai` use the environment variable's value

#### Scenario: No keychain on a headless Linux host
- **WHEN** the user runs `harness auth add openrouter` and no keychain service is available
- **THEN** the key is written to `credentials.json` in the data directory with mode 0600 and a warning is shown

#### Scenario: Key piped from a password manager
- **WHEN** the user runs `pass show openai | harness auth add openai`
- **THEN** the key is stored without its trailing newline, and it never appears in the command line or the shell history

#### Scenario: The keychain refuses a rotated key
- **WHEN** the keychain holds a key for `openai` and refuses to store the one the user adds to replace it
- **THEN** requests to `openai` use the new key, or `harness auth add` fails saying the keychain still holds the older one

### Requirement: Multiple account profiles per provider
The system SHALL store credentials per provider and named account profile, with `default` as the unnamed profile. `harness login <provider> --profile <name>` and `harness auth add <provider> --profile <name>` MUST store credentials under that profile, and `harness auth use <provider> <name>` MUST make that profile the active one for the provider.

#### Scenario: Switching ChatGPT accounts
- **WHEN** the user has signed in with profiles `personal` and `work` and runs `harness auth use chatgpt work`
- **THEN** subsequent `chatgpt/*` requests use the `work` account's credentials

#### Scenario: A damaged accounts file
- **WHEN** `accounts.toml` cannot be read and the user selects a `chatgpt/*` model
- **THEN** harness reports the damaged file and how to recover, and sends no request as the `default` profile's account

### Requirement: ChatGPT sign-in
The system SHALL provide `harness login chatgpt`, which signs the user in through a browser-based OAuth flow with PKCE and a localhost callback, and SHALL offer a device-code flow when `--device` is given or a browser cannot be opened. The access and refresh tokens MUST be stored in the credential store, with the account id read from the ID token (the ID token itself is not stored), and refreshed automatically before expiry or after a single 401 response. A refresh MUST hold a lock shared by every harness process for that profile, and, once it holds it, MUST read the stored tokens again and use them when another process has already refreshed them. Refreshed tokens MUST be used even when they cannot be stored, with a warning. Only the authorization server refusing the refresh token MUST lead to signing in again; an unavailable server MUST leave the tokens as they are and fail the request as retryable. The provider name `chatgpt` MUST be reserved for the signed-in account: a configuration file that defines it MUST be refused with an error naming the file, and the stored sign-in MUST never be sent as an API key. The login flow MUST tell the user that ChatGPT subscription use in third-party tools relies on OpenAI's current practice rather than a contractual guarantee, and that harness identifies to OpenAI as the Codex CLI.

#### Scenario: Sign-in over SSH
- **WHEN** the user runs `harness login chatgpt` in an SSH session without a browser
- **THEN** the system displays a device code and verification URL and completes sign-in once the user approves

#### Scenario: Expired access token
- **WHEN** a request with the stored ChatGPT token returns 401 and the refresh token is valid
- **THEN** the system refreshes the token, retries the request once, and the turn continues

#### Scenario: Two sessions refresh at once
- **WHEN** another harness process refreshed the stored ChatGPT tokens after this process read them, and this process's request returns 401
- **THEN** this process uses the stored tokens without asking the authorization server again

#### Scenario: Two sessions find the token expired at the same moment
- **WHEN** two harness processes find the stored access token about to expire at the same time
- **THEN** only one of them asks the authorization server for new tokens, and the other uses the tokens it stored

#### Scenario: Refreshed tokens cannot be stored
- **WHEN** a refresh succeeds but the credential store refuses the new tokens
- **THEN** the request goes on with the new tokens and the user is warned

#### Scenario: The authorization server is unavailable
- **WHEN** a refresh is answered with a server error, a 429, or not at all
- **THEN** the request fails with a retryable error, the stored tokens are kept, and the user is not told to sign in again

#### Scenario: A configuration defines the chatgpt provider
- **WHEN** a global or project `config.toml` defines `[providers.chatgpt]`
- **THEN** harness refuses the configuration with an error naming the file, and sends the stored sign-in nowhere

### Requirement: Credentials can be removed
The system SHALL provide `harness logout <provider> [--profile <name>]`, which removes that provider's stored credentials for the given profile (default: the active profile) from the keychain and the credentials file, whichever store is chosen. When the keychain refuses to remove them, the command MUST report it and exit with a non-zero code.

#### Scenario: Logout
- **WHEN** the user runs `harness logout chatgpt` with only the default profile signed in
- **THEN** subsequent `chatgpt/*` requests report that the user is not signed in

#### Scenario: The keychain refuses to remove a key
- **WHEN** the user runs `harness logout openai` and the keychain refuses to delete the stored key
- **THEN** harness reports the refusal and exits with code 1, rather than reporting the key removed

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
The system MUST NOT write the API keys, OAuth access tokens, or refresh tokens it holds to logs, session files, tool-output files, NDJSON output, error messages, or anything else it prints, whether they appear whole in one place, in pieces across streamed deltas, or JSON-escaped inside a tool call's arguments. They are those in the environment (each configured provider's key variable included), every one in the credential file, whichever provider and profile it belongs to, and those read from the keychain or refreshed during the run; the keychain is not read only to learn keys the run does not use. The values of environment variables whose names end in `KEY`, `TOKEN`, `SECRET`, `PASSWORD` or their plurals, `PASSPHRASE`, `CREDENTIALS`, `_PASS` or `_PWD`, and the password of any URL an environment variable holds, MUST be treated as secrets too, when they are eight characters or longer.

#### Scenario: Debug logging
- **WHEN** a turn runs with `--debug` using an API key provider
- **THEN** neither the log file nor the session file contains the key's value

#### Scenario: A command prints the environment
- **WHEN** the model runs `printenv` through the `bash` tool while `OPENAI_API_KEY` is set
- **THEN** the session file, the tool-output files and the NDJSON output show `[redacted]` in place of the key's value

#### Scenario: The model repeats a key in a streamed answer
- **WHEN** the model's streamed answer holds an API key split across several deltas
- **THEN** the NDJSON output and the debug log show `[redacted]` in its place, and no part of the key

#### Scenario: A password in a tool call
- **WHEN** the model runs a command holding the value of `DB_PASSWORD`, which contains a quote and a backslash
- **THEN** neither the session file, the NDJSON output, the debug log nor what harness prints on stderr contains it
