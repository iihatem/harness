//! `harness auth add`, `harness auth use` and `harness logout`: stored API keys and account
//! profiles. A key is read from standard input, never from the command line, where it would
//! reach the shell history.

use std::io::{BufRead, IsTerminal, Read};

use harness_config::config::Protocol;
use harness_providers::{
    credentials::{self, CredentialError},
    registry::{self, BUILTIN_PROVIDERS, ResolveError},
};

use crate::{
    setup::{self, Setup},
    term::terminal_safe,
};

/// What a provider needs to be used.
enum Needs {
    /// An API key, sent over this protocol.
    Key(Protocol),
    /// Signing in (`harness login`).
    SignIn,
    /// Nothing: a local server.
    Nothing,
}

/// What `provider` needs, or `None` when there is no such provider.
fn needs(setup: &Setup, provider: &str) -> Option<Needs> {
    // Reserved: no configuration can make it a provider that takes a key.
    if provider == registry::CHATGPT {
        return Some(Needs::SignIn);
    }
    if let Some(cfg) = setup.config.providers.get(provider) {
        return Some(match cfg.api_key_env {
            Some(_) => Needs::Key(cfg.protocol),
            None => Needs::Nothing,
        });
    }
    let builtin = BUILTIN_PROVIDERS.iter().find(|b| b.name == provider)?;
    Some(match builtin.key_env {
        Some(_) => Needs::Key(builtin.protocol),
        None => Needs::Nothing,
    })
}

fn load() -> Result<Setup, u8> {
    setup::load().map_err(|message| {
        eprintln!("error: {}", terminal_safe(&message));
        2
    })
}

/// Checks the provider and profile names, printing why one is not valid.
fn check_names(provider: &str, profile: &str) -> Result<(), u8> {
    credentials::check_name("provider", provider)
        .and_then(|()| credentials::check_name("profile", profile))
        .map_err(|e| {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
            2
        })
}

fn unknown(provider: &str) -> u8 {
    eprintln!(
        "error: unknown provider `{}`; define it under [providers.{}] in config.toml",
        terminal_safe(provider),
        terminal_safe(provider)
    );
    2
}

/// `harness auth add <provider> [--profile <name>]`.
pub fn add(provider: &str, profile: &str) -> u8 {
    let setup = match load() {
        Ok(setup) => setup,
        Err(code) => return code,
    };
    if let Err(code) = check_names(provider, profile) {
        return code;
    }
    let protocol = match needs(&setup, provider) {
        None => return unknown(provider),
        Some(Needs::SignIn) => {
            eprintln!(
                "error: {provider} takes no API key; sign in with `harness login {provider}`"
            );
            return 2;
        }
        Some(Needs::Nothing) => {
            eprintln!(
                "error: {provider} needs no API key; to give it one, set `api_key_env` under [providers.{provider}] in config.toml"
            );
            return 2;
        }
        Some(Needs::Key(protocol)) => protocol,
    };
    let key = match read_key(provider) {
        Ok(key) => key,
        Err(e) => {
            eprintln!("error: cannot read the key: {e}");
            return 1;
        }
    };
    if key.is_empty() {
        eprintln!("error: no API key was given on standard input");
        return 2;
    }
    if protocol == Protocol::AnthropicMessages && registry::is_claude_subscription_token(&key) {
        let refused = ResolveError::SubscriptionToken {
            provider: provider.to_string(),
        };
        eprintln!("error: {refused}");
        return 2;
    }
    match setup.credentials.set(provider, profile, &key) {
        Ok(place) => {
            for warning in setup.credentials.take_warnings() {
                eprintln!("warning: {}", terminal_safe(&warning));
            }
            println!(
                "Stored the API key for {provider} (profile {profile}) in {}.",
                terminal_safe(&place)
            );
            0
        }
        Err(e) => fail(e),
    }
}

/// `harness auth use <provider> <profile>`.
pub fn use_profile(provider: &str, profile: &str) -> u8 {
    let setup = match load() {
        Ok(setup) => setup,
        Err(code) => return code,
    };
    if let Err(code) = check_names(provider, profile) {
        return code;
    }
    let needs = match needs(&setup, provider) {
        None => return unknown(provider),
        Some(needs) => needs,
    };
    if let Err(e) = setup.credentials.use_profile(provider, profile) {
        return fail(e);
    }
    println!("{provider} now uses profile {profile}.");
    if let Ok(None) = setup.credentials.get(provider, profile) {
        let how = match needs {
            Needs::SignIn => format!("harness login {provider} --profile {profile}"),
            _ => format!("harness auth add {provider} --profile {profile}"),
        };
        println!("Nothing is stored under it yet: run `{how}`.");
    }
    0
}

/// `harness logout <provider> [--profile <name>]`: the named profile, or the one in use.
pub fn logout(provider: &str, profile: Option<&str>) -> u8 {
    let setup = match load() {
        Ok(setup) => setup,
        Err(code) => return code,
    };
    let profile = match profile {
        Some(profile) => profile.to_string(),
        None => match setup.credentials.active_profile(provider) {
            Ok(profile) => profile,
            Err(e) => return fail(e),
        },
    };
    if let Err(code) = check_names(provider, &profile) {
        return code;
    }
    match setup.credentials.remove(provider, &profile) {
        Ok(true) => {
            println!("Removed the stored credentials for {provider} (profile {profile}).");
            0
        }
        Ok(false) => {
            println!("No credentials are stored for {provider} (profile {profile}).");
            0
        }
        Err(e) => fail(e),
    }
}

fn fail(error: CredentialError) -> u8 {
    eprintln!("error: {}", terminal_safe(&error.to_string()));
    match error {
        CredentialError::BadName { .. } => 2,
        _ => 1,
    }
}

/// The key on standard input: typed without echo on a terminal, or the first line of what is
/// piped in (a password manager may print more lines after it), without surrounding whitespace.
fn read_key(provider: &str) -> std::io::Result<String> {
    let stdin = std::io::stdin();
    let text = if stdin.is_terminal() {
        eprint!("API key for {provider} (not shown): ");
        let line = read_hidden_line()?;
        eprintln!();
        line
    } else {
        let mut text = String::new();
        stdin.lock().read_to_string(&mut text)?;
        text
    };
    Ok(text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string())
}

/// One line from the terminal, with echo turned off while it is typed.
fn read_hidden_line() -> std::io::Result<String> {
    use nix::sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr};
    let stdin = std::io::stdin();
    let saved = tcgetattr(&stdin)?;
    let mut quiet = saved.clone();
    quiet.local_flags.remove(LocalFlags::ECHO);
    tcsetattr(&stdin, SetArg::TCSANOW, &quiet)?;
    let mut line = String::new();
    let read = stdin.lock().read_line(&mut line);
    let _ = tcsetattr(&stdin, SetArg::TCSANOW, &saved);
    read.map(|_| line)
}
