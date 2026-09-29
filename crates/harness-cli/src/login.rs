//! `harness login <provider>`, and `/login` inside the session: ChatGPT sign-in, in the browser
//! or with a device code. Claude subscriptions cannot be signed in to: Anthropic allows them
//! only in Claude Code.

use harness_providers::registry::{BUILTIN_PROVIDERS, auth_add_command};
use tokio_util::sync::CancellationToken;

use crate::{setup::Setup, term::terminal_safe};

/// What a sign-in says while it runs: the notice, the address to open, the device code.
pub type Say = dyn Fn(String) + Send + Sync;

/// Why a sign-in did not happen: the message, and `harness login`'s exit code.
#[derive(Debug)]
pub struct Failed {
    pub code: u8,
    pub message: String,
}

fn failed(code: u8, message: String) -> Failed {
    Failed { code, message }
}

/// What every ChatGPT sign-in says first.
#[cfg(feature = "chatgpt-login")]
pub const NOTICE: &str = "Signing in with ChatGPT lets harness use the models your ChatGPT plan includes. OpenAI allows this in third-party tools today, but that is its current practice, not a contractual guarantee: it can change at any time. harness signs in with the Codex CLI's OAuth client and identifies to OpenAI as the Codex CLI.";

/// `harness login <provider> [--profile <name>] [--device]`.
pub async fn run(provider: &str, profile: &str, device: bool) -> u8 {
    let setup = match crate::setup::load() {
        Ok(setup) => setup,
        Err(message) => {
            eprintln!("error: {}", terminal_safe(&message));
            return 2;
        }
    };
    let code = login(&setup, provider, profile, device).await;
    setup.print_credential_warnings();
    code
}

async fn login(setup: &Setup, provider: &str, profile: &str, device: bool) -> u8 {
    if let Err(e) = harness_providers::credentials::check_name("profile", profile) {
        eprintln!("error: {}", terminal_safe(&e.to_string()));
        return 2;
    }
    if provider != "chatgpt" {
        eprintln!("error: {}", refusal(setup, provider, profile));
        return 2;
    }
    // Ctrl+C stops the sign-in.
    let cancel = CancellationToken::new();
    let on_ctrl_c = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            on_ctrl_c.cancel();
        }
    });
    let device = device || wants_device_flow(crate::setup::env);
    let say = |text: String| eprintln!("{text}");
    match sign_in(setup, profile, device, &say, cancel).await {
        Ok(done) => {
            setup.print_credential_warnings();
            println!("{done}");
            println!("Use a model your plan includes with --model chatgpt/<model>.");
            0
        }
        Err(Failed { code: 130, message }) => {
            eprintln!("{message}");
            130
        }
        Err(Failed { code, message }) => {
            eprintln!("error: {message}");
            code
        }
    }
}

/// `/login [provider] [--device]` inside the session: ChatGPT sign-in, under the profile ChatGPT
/// uses now; what it says goes to `say`, and `cancel` (Esc) stops it. Ok: what to tell the
/// user. Other providers are refused as `harness login` refuses them.
pub async fn in_session(
    setup: &Setup,
    provider: &str,
    device: bool,
    say: &Say,
    cancel: CancellationToken,
) -> Result<String, String> {
    if provider != "chatgpt" {
        return Err(refusal(setup, provider, "default"));
    }
    let profile = setup
        .credentials
        .active_profile(harness_providers::registry::CHATGPT)
        .map_err(|e| e.to_string())?;
    let device = device || wants_device_flow(|var| (setup.env)(var));
    sign_in(setup, &profile, device, say, cancel)
        .await
        .map_err(|failed| failed.message)
}

/// Why `provider`, which is not `chatgpt`, cannot be signed in to (`profile` for the hint).
fn refusal(setup: &Setup, provider: &str, profile: &str) -> String {
    match provider {
        "anthropic" | "claude" => format!(
            "harness cannot sign in to Claude: Anthropic allows Claude Free, Pro and Max plans only in Claude Code. Use an Anthropic API key instead: `{}`.",
            auth_add_command("anthropic", profile)
        ),
        other if takes_a_key(setup, other) => format!(
            "{other} takes an API key, not a sign-in: `{}`",
            auth_add_command(other, profile)
        ),
        other
            if setup.config.providers.contains_key(other)
                || BUILTIN_PROVIDERS.iter().any(|b| b.name == other) =>
        {
            format!("{other} needs no sign-in")
        }
        other => format!(
            "unknown provider `{}`; `harness login` signs in to chatgpt",
            terminal_safe(other)
        ),
    }
}

#[cfg(not(feature = "chatgpt-login"))]
async fn sign_in(
    _setup: &Setup,
    _profile: &str,
    _device: bool,
    _say: &Say,
    _cancel: CancellationToken,
) -> Result<String, Failed> {
    Err(failed(
        2,
        "this build of harness was made without ChatGPT sign-in (the `chatgpt-login` feature)"
            .into(),
    ))
}

/// Signs in to ChatGPT under `profile`, with a device code when `device` is set, and stores the
/// tokens, which harness keeps out of everything it shows from then on. Ok: what to tell the
/// user.
#[cfg(feature = "chatgpt-login")]
async fn sign_in(
    setup: &Setup,
    profile: &str,
    device: bool,
    say: &Say,
    cancel: CancellationToken,
) -> Result<String, Failed> {
    use harness_providers::{
        chatgpt::oauth::{ISSUER, OAuth},
        registry::CHATGPT,
    };
    say(NOTICE.to_string());
    // A test hook, in debug builds only: a mock authorization server.
    let issuer =
        harness_providers::registry::test_hook("HARNESS_CHATGPT_ISSUER", |var| (setup.env)(var))
            .unwrap_or_else(|| ISSUER.to_string());
    let oauth = OAuth::new(&issuer).map_err(|e| failed(2, terminal_safe(&e.to_string())))?;
    let tokens = tokio::select! {
        tokens = flows::sign_in(&oauth, device, say) => tokens,
        _ = cancel.cancelled() => return Err(failed(130, "sign-in cancelled".into())),
    };
    let tokens = tokens.map_err(|e| failed(1, terminal_safe(&e.to_string())))?;
    // A renewal in flight in another run would store its tokens over these.
    let renewing = setup.credentials.lock_renewal(
        CHATGPT,
        profile,
        "storing the new ChatGPT sign-in",
        |note| say(format!("note: {}", terminal_safe(&note))),
    );
    let renewing = tokio::select! {
        lock = renewing => lock,
        _ = cancel.cancelled() => return Err(failed(130, "sign-in cancelled".into())),
    };
    // What harness shows from now on leaves the new tokens out.
    setup.redactor.add(&tokens.access_token);
    setup.redactor.add(&tokens.refresh_token);
    let stored = setup.credentials.set(CHATGPT, profile, &tokens.to_json());
    drop(renewing);
    let place = stored_at(stored)?;
    let who = tokens
        .email
        .as_deref()
        .map(|email| format!(" as {}", terminal_safe(email)))
        .unwrap_or_default();
    Ok(format!(
        "Signed in to ChatGPT{who} (profile {profile}); the tokens are in {}.",
        terminal_safe(&place)
    ))
}

/// Where the new tokens are, or why the sign-in failed: a keychain store that could not remove an
/// older file copy has not replaced the credential everywhere it is read from, which is a
/// failure whose fix the warnings queued by `Credentials::set` give.
#[cfg(feature = "chatgpt-login")]
fn stored_at(
    stored: Result<
        harness_providers::credentials::Stored,
        harness_providers::credentials::CredentialError,
    >,
) -> Result<String, Failed> {
    let stored = stored.map_err(|e| failed(1, terminal_safe(&e.to_string())))?;
    if stored.stale_file_copy {
        return Err(failed(
            1,
            harness_providers::credentials::STALE_FILE_COPY.to_string(),
        ));
    }
    Ok(stored.place)
}

/// Whether `provider` authenticates with an API key.
fn takes_a_key(setup: &Setup, provider: &str) -> bool {
    match setup.config.providers.get(provider) {
        Some(cfg) => cfg.api_key_env.is_some(),
        None => BUILTIN_PROVIDERS
            .iter()
            .any(|b| b.name == provider && b.key_env.is_some()),
    }
}

/// Whether no browser can be opened here: over SSH, or on Linux without a display.
pub fn wants_device_flow(env: impl Fn(&str) -> Option<String>) -> bool {
    let set = |var: &str| env(var).is_some_and(|value| !value.is_empty());
    if set("SSH_CONNECTION") || set("SSH_TTY") {
        return true;
    }
    cfg!(target_os = "linux") && !set("DISPLAY") && !set("WAYLAND_DISPLAY")
}

#[cfg(feature = "chatgpt-login")]
mod flows {
    use std::time::Duration;

    use harness_providers::chatgpt::oauth::{
        CALLBACK_PORTS, CallbackServer, DEVICE_CODE_WAIT, OAuth, OAuthError, Pkce, Tokens,
        random_state,
    };

    use super::Say;
    use crate::term::terminal_safe;

    /// How long the browser flow waits for the user.
    const BROWSER_WAIT: Duration = Duration::from_secs(10 * 60);

    /// Signs in in the browser, or with a device code when `device` is set or no browser opens.
    /// What the user must do goes to `say`.
    pub async fn sign_in(oauth: &OAuth, device: bool, say: &Say) -> Result<Tokens, OAuthError> {
        if !device {
            match browser(oauth, say).await {
                Ok(tokens) => return Ok(tokens),
                Err(Browser::CannotOpen(why)) => say(format!(
                    "cannot open a browser ({}); signing in with a device code instead",
                    terminal_safe(&why)
                )),
                Err(Browser::Failed(e)) => return Err(e),
            }
        }
        let code = oauth.request_device_code().await?;
        say(format!(
            "To sign in, open {} in a browser and enter the code {} (it expires in 15 minutes).\nOnly enter it if you started this sign-in yourself.",
            terminal_safe(&code.verification_url),
            terminal_safe(&code.user_code)
        ));
        oauth.poll_device_code(&code, DEVICE_CODE_WAIT).await
    }

    enum Browser {
        CannotOpen(String),
        Failed(OAuthError),
    }

    impl From<OAuthError> for Browser {
        fn from(error: OAuthError) -> Browser {
            Browser::Failed(error)
        }
    }

    impl From<std::io::Error> for Browser {
        fn from(error: std::io::Error) -> Browser {
            Browser::Failed(error.into())
        }
    }

    async fn browser(oauth: &OAuth, say: &Say) -> Result<Tokens, Browser> {
        let callback = CallbackServer::bind(&CALLBACK_PORTS).await?;
        let pkce = Pkce::generate()?;
        let state = random_state()?;
        let url = oauth.authorize_url(&callback.redirect_uri(), &pkce, &state);
        open(&url)
            .await
            .map_err(|e| Browser::CannotOpen(e.to_string()))?;
        say(waiting_message(&url));
        let code = tokio::time::timeout(BROWSER_WAIT, callback.wait_for_code(&state))
            .await
            .map_err(|_| OAuthError::TimedOut)??;
        Ok(oauth
            .exchange_code(&code, &callback.redirect_uri(), &pkce.verifier)
            .await?)
    }

    /// What the browser flow says while it waits for the user at `url`.
    pub fn waiting_message(url: &str) -> String {
        format!(
            "Sign in in the browser window that opened. If none did, open:\n  {}\nharness waits 10 minutes for the browser to come back to 127.0.0.1. Where a browser cannot reach this machine's 127.0.0.1 (in a container, a remote editor), stop (Ctrl+C, or Esc in a session) and sign in with a device code instead: add --device.",
            terminal_safe(url)
        )
    }

    /// Opens `url` in the default browser.
    async fn open(url: &str) -> std::io::Result<()> {
        let program = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let status = tokio::process::Command::new(program)
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!("{program} failed: {status}")))
        }
    }
}

#[cfg(all(test, feature = "chatgpt-login"))]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |var| {
            pairs
                .iter()
                .find(|(k, _)| *k == var)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn ssh_sessions_use_the_device_flow() {
        assert!(wants_device_flow(env(&[("SSH_CONNECTION", "a b c d")])));
        assert!(wants_device_flow(env(&[("SSH_TTY", "/dev/pts/1")])));
    }

    // Review C, M11.
    #[test]
    fn the_browser_wait_offers_the_device_flow() {
        let message = flows::waiting_message("https://auth.example/oauth/authorize?x=1");
        assert!(message.contains("10 minutes"), "{message}");
        assert!(message.contains("--device"), "{message}");
    }

    #[test]
    fn a_display_decides_on_linux() {
        let with_display = wants_device_flow(env(&[("DISPLAY", ":0")]));
        assert!(!with_display);
        assert!(!wants_device_flow(env(&[("WAYLAND_DISPLAY", "wayland-0")])));
        // macOS always has a browser; Linux without a display has none.
        assert_eq!(wants_device_flow(env(&[])), cfg!(target_os = "linux"));
    }

    // Final review, wave 5 re-review R1: a keychain store that could not remove an older file
    // copy has not actually replaced the credential everywhere it is read from, so `harness
    // login` must fail loudly rather than report success.
    #[test]
    fn a_stale_file_copy_after_signing_in_fails_loudly() {
        let stored = Ok(harness_providers::credentials::Stored {
            place: "the keychain".into(),
            stale_file_copy: true,
        });
        assert_eq!(stored_at(stored).unwrap_err().code, 1);
    }

    #[test]
    fn a_clean_sign_in_reports_success() {
        let stored = Ok(harness_providers::credentials::Stored {
            place: "the keychain".into(),
            stale_file_copy: false,
        });
        assert_eq!(stored_at(stored).unwrap(), "the keychain");
    }

    #[test]
    fn a_credential_error_signing_in_is_a_plain_failure() {
        let stored = Err(harness_providers::credentials::CredentialError::Keychain(
            "the collection is locked".into(),
        ));
        assert_eq!(stored_at(stored).unwrap_err().code, 1);
    }
}
