//! `harness auth add`, `harness auth use` and `harness logout`: stored API keys and account
//! profiles. A key is read from standard input, never from the command line, where it would
//! reach the shell history.

use std::io::{BufRead, IsTerminal};

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
    /// An API key, which this environment variable gives too.
    Key(String),
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
        return Some(match &cfg.api_key_env {
            Some(var) => Needs::Key(var.clone()),
            None => Needs::Nothing,
        });
    }
    let builtin = BUILTIN_PROVIDERS.iter().find(|b| b.name == provider)?;
    Some(match builtin.key_env {
        Some(var) => Needs::Key(var.to_string()),
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
    let code = add_key(&setup, provider, profile);
    setup.print_credential_warnings();
    code
}

fn add_key(setup: &Setup, provider: &str, profile: &str) -> u8 {
    if let Err(code) = check_names(provider, profile) {
        return code;
    }
    let var = match needs(setup, provider) {
        None => return unknown(provider),
        Some(Needs::SignIn) => {
            eprintln!(
                "error: {provider} takes no API key; {}",
                registry::sign_in_hint(provider, profile)
            );
            return 2;
        }
        Some(Needs::Nothing) => {
            eprintln!(
                "error: {provider} needs no API key; to give it one, set `api_key_env` under [providers.{provider}] in config.toml"
            );
            return 2;
        }
        Some(Needs::Key(var)) => var,
    };
    let key = match read_key(provider) {
        Ok(key) => key,
        Err(e) => {
            eprintln!("error: cannot read the key: {e}");
            return e.exit_code();
        }
    };
    if key.is_empty() {
        eprintln!("error: no API key was given on standard input");
        return 2;
    }
    // Never a valid key for any provider, whatever its protocol.
    if registry::is_claude_subscription_token(&key) {
        let refused = ResolveError::SubscriptionToken {
            provider: provider.to_string(),
        };
        eprintln!("error: {refused}");
        return 2;
    }
    match setup.credentials.set(provider, profile, &key) {
        Ok(place) => {
            println!(
                "Stored the API key for {provider} (profile {profile}) in {}.",
                terminal_safe(&place)
            );
            if setup::env(&var).is_some_and(|value| !value.is_empty()) {
                eprintln!(
                    "note: ${} is set, and wins over the stored key while it is",
                    terminal_safe(&var)
                );
            }
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
    let code = choose(&setup, provider, profile);
    setup.print_credential_warnings();
    code
}

fn choose(setup: &Setup, provider: &str, profile: &str) -> u8 {
    if let Err(code) = check_names(provider, profile) {
        return code;
    }
    let how = match needs(setup, provider) {
        None => return unknown(provider),
        Some(Needs::Nothing) => {
            eprintln!(
                "error: {provider} needs no credentials, so it has no profiles to choose from"
            );
            return 2;
        }
        Some(Needs::SignIn) => registry::sign_in_hint(provider, profile),
        Some(Needs::Key(_)) => format!("run `{}`", registry::auth_add_command(provider, profile)),
    };
    if let Err(e) = setup.credentials.use_profile(provider, profile) {
        return fail(e);
    }
    println!("{provider} now uses profile {profile}.");
    if let Ok(None) = setup.credentials.get(provider, profile) {
        println!("Nothing is stored under it yet; {how}.");
    }
    0
}

/// `harness logout <provider> [--profile <name>]`: the named profile, or the one in use.
pub fn logout(provider: &str, profile: Option<&str>) -> u8 {
    let setup = match load() {
        Ok(setup) => setup,
        Err(code) => return code,
    };
    let code = remove(&setup, provider, profile);
    setup.print_credential_warnings();
    code
}

fn remove(setup: &Setup, provider: &str, profile: Option<&str>) -> u8 {
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

/// The most a key read from a pipe may take up: far more than any API key.
const MAX_KEY_BYTES: u64 = 64 * 1024;

/// Why no key could be read.
#[derive(Debug)]
enum KeyError {
    Io(std::io::Error),
    /// The input is not UTF-8.
    NotText,
    /// No line ended within [`MAX_KEY_BYTES`].
    TooLong,
}

impl KeyError {
    /// Bad input is a usage error (2); a failure to read, an I/O one (1).
    fn exit_code(&self) -> u8 {
        match self {
            KeyError::Io(_) => 1,
            KeyError::NotText | KeyError::TooLong => 2,
        }
    }
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyError::Io(e) => write!(f, "{e}"),
            KeyError::NotText => write!(f, "it is not UTF-8 text"),
            KeyError::TooLong => write!(
                f,
                "its first line is too long (over {} KiB)",
                MAX_KEY_BYTES / 1024
            ),
        }
    }
}

impl From<std::io::Error> for KeyError {
    fn from(error: std::io::Error) -> KeyError {
        if error.kind() == std::io::ErrorKind::InvalidData {
            KeyError::NotText
        } else {
            KeyError::Io(error)
        }
    }
}

/// The key on standard input: typed without echo on a terminal, or the first line with
/// something on it of what is piped in (a password manager may print more lines after it, and
/// may keep the pipe open), without surrounding whitespace or a byte-order mark.
fn read_key(provider: &str) -> Result<String, KeyError> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        eprint!("API key for {provider} (not shown): ");
        let line = read_hidden_line()?;
        eprintln!();
        return Ok(clean(&line).to_string());
    }
    first_key_line(stdin.lock())
}

/// `line` without surrounding whitespace or a byte-order mark.
fn clean(line: &str) -> &str {
    line.trim_matches(|c: char| c == '\u{feff}' || c.is_whitespace())
}

/// The first line of `input` with something on it, read no further than that line and never
/// past [`MAX_KEY_BYTES`].
fn first_key_line(input: impl BufRead) -> Result<String, KeyError> {
    let mut input = input.take(MAX_KEY_BYTES);
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            // The end of the input, or of what may be read.
            return if input.limit() == 0 {
                Err(KeyError::TooLong)
            } else {
                Ok(String::new())
            };
        }
        if !line.ends_with(b"\n") && input.limit() == 0 {
            return Err(KeyError::TooLong);
        }
        let text = std::str::from_utf8(&line).map_err(|_| KeyError::NotText)?;
        let key = clean(text);
        if !key.is_empty() {
            return Ok(key.to_string());
        }
    }
}

/// One line from the terminal, with echo turned off while it is typed. Should harness be
/// interrupted (Ctrl+C) or ended meanwhile, the terminal gets its settings back first.
fn read_hidden_line() -> std::io::Result<String> {
    use nix::sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr};
    let stdin = std::io::stdin();
    let saved = tcgetattr(&stdin)?;
    let mut quiet = saved.clone();
    quiet.local_flags.remove(LocalFlags::ECHO);
    let restorer = restore_on_signal::Guard::new(&saved);
    tcsetattr(&stdin, SetArg::TCSANOW, &quiet)?;
    let mut line = String::new();
    let read = stdin.lock().read_line(&mut line);
    let _ = tcsetattr(&stdin, SetArg::TCSANOW, &saved);
    drop(restorer);
    read.map(|_| line)
}

/// Gives the terminal its settings back when a signal ends harness while echo is off.
mod restore_on_signal {
    use std::sync::atomic::{AtomicPtr, Ordering};

    use nix::libc;
    use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
    use nix::sys::termios::Termios;

    /// The settings to restore, for the handler, which may only do async-signal-safe things.
    static SAVED: AtomicPtr<libc::termios> = AtomicPtr::new(std::ptr::null_mut());

    /// The signals that end harness at a prompt: Ctrl+C, Ctrl+\, a closed terminal, `kill`.
    const SIGNALS: [Signal; 4] = [
        Signal::SIGINT,
        Signal::SIGQUIT,
        Signal::SIGHUP,
        Signal::SIGTERM,
    ];

    extern "C" fn restore_and_end(signal: libc::c_int) {
        let saved = SAVED.load(Ordering::SeqCst);
        // SAFETY: tcsetattr, signal and raise are async-signal-safe; `saved`, when not null,
        // points to settings that live until the guard is dropped, which takes it back first.
        unsafe {
            if !saved.is_null() {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, saved);
            }
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
    }

    /// While it lives, those signals restore `saved` before they end harness as they would have.
    pub struct Guard {
        previous: Vec<(Signal, SigAction)>,
    }

    impl Guard {
        pub fn new(saved: &Termios) -> Guard {
            let raw: libc::termios = saved.clone().into();
            SAVED.store(Box::into_raw(Box::new(raw)), Ordering::SeqCst);
            let action = SigAction::new(
                SigHandler::Handler(restore_and_end),
                SaFlags::empty(),
                SigSet::empty(),
            );
            let previous = SIGNALS
                .iter()
                .filter_map(|&signal| {
                    // SAFETY: the handler only restores the terminal and ends the process.
                    unsafe { sigaction(signal, &action) }
                        .ok()
                        .map(|old| (signal, old))
                })
                .collect();
            Guard { previous }
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            for (signal, old) in &self.previous {
                // SAFETY: puts back the action that was there before.
                let _ = unsafe { sigaction(*signal, old) };
            }
            let saved = SAVED.swap(std::ptr::null_mut(), Ordering::SeqCst);
            if !saved.is_null() {
                // SAFETY: made by Box::into_raw in `new`, and no handler can see it any more.
                drop(unsafe { Box::from_raw(saved) });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_the_first_line_with_something_on_it() {
        let key = |input: &[u8]| first_key_line(input).map_err(|e| e.to_string());
        assert_eq!(key(b"\n  \r\n\xef\xbb\xbfsk-1 \r\nsk-2\n").unwrap(), "sk-1");
        assert_eq!(key(b"sk-no-newline").unwrap(), "sk-no-newline");
        assert_eq!(key(b"").unwrap(), "");
        assert!(key(b"\xff\xfe\n").unwrap_err().contains("UTF-8"));
        let long = vec![b'k'; MAX_KEY_BYTES as usize + 1];
        assert!(key(&long).unwrap_err().contains("too long"));
        // Blank lines count towards the limit too.
        let blank = vec![b'\n'; MAX_KEY_BYTES as usize + 1];
        assert!(key(&blank).unwrap_err().contains("too long"));
    }
}
