//! `harness login <provider>`: ChatGPT sign-in, in the browser or with a device code. Claude
//! subscriptions cannot be signed in to: Anthropic allows them only in Claude Code.

use harness_providers::registry::{BUILTIN_PROVIDERS, auth_add_command};

use crate::{setup::Setup, term::terminal_safe};

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
    match provider {
        "chatgpt" => sign_in(setup, profile, device).await,
        "anthropic" | "claude" => {
            eprintln!(
                "error: harness cannot sign in to Claude: Anthropic allows Claude Free, Pro and Max plans only in Claude Code. Use an Anthropic API key instead: `{}`.",
                auth_add_command("anthropic", profile)
            );
            2
        }
        other if takes_a_key(setup, other) => {
            eprintln!(
                "error: {other} takes an API key, not a sign-in: `{}`",
                auth_add_command(other, profile)
            );
            2
        }
        other
            if setup.config.providers.contains_key(other)
                || BUILTIN_PROVIDERS.iter().any(|b| b.name == other) =>
        {
            eprintln!("error: {other} needs no sign-in");
            2
        }
        other => {
            eprintln!(
                "error: unknown provider `{}`; `harness login` signs in to chatgpt",
                terminal_safe(other)
            );
            2
        }
    }
}

#[cfg(not(feature = "chatgpt-login"))]
async fn sign_in(_setup: &Setup, _profile: &str, _device: bool) -> u8 {
    eprintln!(
        "error: this build of harness was made without ChatGPT sign-in (the `chatgpt-login` feature)"
    );
    2
}

#[cfg(feature = "chatgpt-login")]
async fn sign_in(setup: &Setup, profile: &str, device: bool) -> u8 {
    use harness_providers::{
        chatgpt::oauth::{ISSUER, OAuth},
        registry::CHATGPT,
    };
    eprintln!("{NOTICE}");
    // A test hook, in debug builds only: a mock authorization server.
    let issuer =
        harness_providers::registry::test_hook("HARNESS_CHATGPT_ISSUER", crate::setup::env)
            .unwrap_or_else(|| ISSUER.to_string());
    let oauth = match OAuth::new(&issuer) {
        Ok(oauth) => oauth,
        Err(e) => {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
            return 2;
        }
    };
    let device = device || wants_device_flow(crate::setup::env);
    let tokens = tokio::select! {
        tokens = flows::sign_in(&oauth, device) => tokens,
        _ = tokio::signal::ctrl_c() => {
            eprintln!("sign-in cancelled");
            return 130;
        }
    };
    let tokens = match tokens {
        Ok(tokens) => tokens,
        Err(e) => {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
            return 1;
        }
    };
    match setup.credentials.set(CHATGPT, profile, &tokens.to_json()) {
        Ok(place) => {
            setup.print_credential_warnings();
            let who = tokens
                .email
                .as_deref()
                .map(|email| format!(" as {}", terminal_safe(email)))
                .unwrap_or_default();
            println!(
                "Signed in to ChatGPT{who} (profile {profile}); the tokens are in {}.",
                terminal_safe(&place)
            );
            println!("Use a model your plan includes with --model chatgpt/<model>.");
            0
        }
        Err(e) => {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
            1
        }
    }
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
#[cfg(feature = "chatgpt-login")]
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

    use crate::term::terminal_safe;

    /// How long the browser flow waits for the user.
    const BROWSER_WAIT: Duration = Duration::from_secs(10 * 60);

    /// Signs in in the browser, or with a device code when `device` is set or no browser opens.
    pub async fn sign_in(oauth: &OAuth, device: bool) -> Result<Tokens, OAuthError> {
        if !device {
            match browser(oauth).await {
                Ok(tokens) => return Ok(tokens),
                Err(Browser::CannotOpen(why)) => eprintln!(
                    "cannot open a browser ({}); signing in with a device code instead",
                    terminal_safe(&why)
                ),
                Err(Browser::Failed(e)) => return Err(e),
            }
        }
        let code = oauth.request_device_code().await?;
        eprintln!(
            "To sign in, open {} in a browser and enter the code {} (it expires in 15 minutes).\nOnly enter it if you started this sign-in yourself.",
            terminal_safe(&code.verification_url),
            terminal_safe(&code.user_code)
        );
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

    async fn browser(oauth: &OAuth) -> Result<Tokens, Browser> {
        let callback = CallbackServer::bind(&CALLBACK_PORTS).await?;
        let pkce = Pkce::generate()?;
        let state = random_state()?;
        let url = oauth.authorize_url(&callback.redirect_uri(), &pkce, &state);
        open(&url)
            .await
            .map_err(|e| Browser::CannotOpen(e.to_string()))?;
        eprintln!("{}", waiting_message(&url));
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
            "Sign in in the browser window that opened. If none did, open:\n  {}\nharness waits 10 minutes for the browser to come back to 127.0.0.1. Where a browser cannot reach this machine's 127.0.0.1 (in a container, a remote editor), press Ctrl+C and sign in with a device code instead: add --device.",
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
}
